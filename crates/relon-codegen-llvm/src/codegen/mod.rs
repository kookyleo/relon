//! IR -> LLVM IR lowering.
//!
//! Phase B widens the emitter past the Phase A bootstrap envelope:
//!
//! - Two entry shapes:
//!   - **Legacy-i64**: `(I64...) -> I64` — driven by
//!     [`LlvmAotEvaluator::from_ir_direct`]. Mirrors the cranelift
//!     crate's same-named envelope; used by the Phase A bootstrap
//!     tests and the side-by-side `from_ir_direct` benchmarks.
//!   - **Buffer-protocol**: `(*state, i32 in_ptr, i32 in_len,
//!     i32 out_ptr, i32 out_cap, i64 caps) -> i32` — driven by
//!     [`LlvmAotEvaluator::from_source`]. Matches what
//!     `lower_workspace_single` emits for every user source.
//!
//! - Op set widened to the W1 / W2 production-source surface:
//!   `LocalGet`, `ConstI64` / `ConstI32` / `ConstBool`, `LetGet` /
//!   `LetSet`, `LoadField` / `StoreField` (scalar slots: I32 / I64 /
//!   F64 / Bool / Null), `Add` / `Sub` / `Mul` / `Div` / `Mod` /
//!   `BitAnd` (`I32` and `I64`), comparison ops (`Eq` / `Ne` /
//!   `Lt` / `Le` / `Gt` / `Ge` — `I32` / `I64` / `Bool` for `Eq`/`Ne`),
//!   structured control flow (`Block` / `Loop` / `Br` / `BrIf` /
//!   `If`), and `Return`.
//!
//! Ops outside the Phase B envelope (stdlib `Call`, pointer-indirect
//! `StoreField`, `MakeClosure`, sandbox-trap helpers, schema-method
//! dispatch, …) surface as [`crate::LlvmError::Codegen`]. They are
//! tracked for Phase C.
//!
//! ## Control-flow lowering vs cranelift
//!
//! Cranelift's `block-with-params` keeps phi nodes implicit (every
//! branch passes the carried values as block arguments). LLVM IR
//! requires explicit `phi` nodes per joining basic block. We avoid
//! both by spilling the IR stack through `alloca` slots whenever
//! control flow joins, and reading them back on the consumer side.
//! That mirrors how a naive byte-code-to-LLVM emitter behaves and
//! relies on LLVM's `mem2reg` pass at -O2/-O3 to turn the alloca
//! reads back into SSA values + phis. For the W1 / W2 hot loops
//! `mem2reg` collapses the alloca traffic into a single
//! loop-carried IR value (verified via `emit_ir_dump`'s output at
//! `-O2`).
//!
//! ## Stack discipline
//!
//! The IR's stack machine carries one value per push. We track the
//! per-op operand stack as `Vec<IntValue>` (every IR value the W1/W2
//! envelope produces fits in an integer type — I32 for Bool / I32-
//! tagged values, I64 for I64-tagged values). The wasm-style "every
//! value above the operand stack is unreachable after `br`" rule
//! lets us drop unconsumed stack slots silently — LLVM's verifier
//! catches missing terminators if we forget to seal a block.

use std::collections::HashMap;

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module as LlvmModule};
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum};
use inkwell::values::{
    BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue,
};
use inkwell::{AddressSpace, IntPredicate};

use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};

use crate::error::LlvmError;
use crate::state::{
    ARENA_STATE_OFFSET_BASE,
    ARENA_STATE_OFFSET_TAIL_CURSOR,
};

// Per-`Op`-family lowering modules. Each holds an
// `impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp>` block with the `emit_*`
// methods for that family; the exhaustive `lower_op` dispatch below
// delegates to them. Mirrors the cranelift backend's `codegen/*`
// split so Phase 0b can fill unimplemented families in place without
// colliding. (Behavior-preserving reorg — Phase 0a.)
mod arith;
mod call;
mod closure;
mod collections;
mod control;
mod mem;
mod schema;
mod string;
mod unicode;

// Family-local enums consumed by the central `lower_op` dispatch.
use arith::BinOp;
use mem::{AbsLoad, AbsStore};

/// Canonical export name the entry function uses in the emitted LLVM
/// module. The evaluator side `dlsym`s / `get_function`s against this
/// symbol after JIT finalize, so renaming it requires touching both
/// crates simultaneously.
pub(crate) const ENTRY_SYMBOL: &str = "relon_llvm_entry";

/// Phase D.1 dispatch-boundary fast path: a second exported entry
/// emitted alongside the buffer-protocol entry whenever the source's
/// `#main(Int...) -> Int` shape qualifies. Skips the HashMap pack +
/// arena round-trip the buffer envelope incurs, dropping the per-call
/// boundary cost from the ~650 ns band into the rust-native ballpark.
///
/// Only resolved when the evaluator's [`FastPathProfile`] is `Some`;
/// the symbol is absent from the JIT module otherwise.
pub(crate) const ENTRY_SYMBOL_FAST: &str = "relon_llvm_entry_fast";

/// Which signature the LLVM emitter should generate. Mirrors the
/// cranelift crate's `EntryShape` enum so a side-by-side comparison
/// of the two backends shares the same vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryShape {
    /// `(I64...) -> I64`. The Phase A bootstrap envelope — used by
    /// `from_ir_direct` callers (tests, helloworld_arith fixtures).
    LegacyI64,
    /// `(*state, i32 in_ptr, i32 in_len, i32 out_ptr, i32 out_cap,
    /// i64 caps) -> i32`. The shape `lower_workspace_single`
    /// synthesises for every user `#main` source. State is the
    /// first parameter to match the cranelift backend's
    /// `BufferEntryFn` layout.
    Buffer,
}

/// Phase D.1 fast-path profile: describes a `#main(Int...) -> Int`
/// source shape eligible for the typed legacy-i64 dispatch fast path.
///
/// The profile maps each declared `#main` Int parameter's buffer
/// offset to the LLVM fast entry's i64 positional slot, and records
/// the offset of the single Int return slot so the trailing
/// `StoreField` can be rewritten into a `ret`. Used exclusively by
/// [`emit_fast_entry`].
#[derive(Debug, Clone)]
pub(crate) struct FastPathProfile {
    /// One entry per declared `#main` arg: the field's byte offset in
    /// the input buffer (matches what `LoadField { offset }` carries
    /// in the IR body) and the i64 slot index in the fast entry
    /// signature. Vector order parallels schema declaration order.
    pub(crate) arg_offsets: Vec<u32>,
    /// Byte offset of the single `value` field in the return buffer.
    /// The trailing `StoreField { offset, ty: I64 }` whose offset
    /// matches this value gets rewritten into a `ret` on the value
    /// (after popping the IR stack normally). Any other `StoreField`
    /// surfaces as an emitter error — the fast path only handles
    /// single-value-wrapper returns.
    pub(crate) ret_offset: u32,
}

/// Phase E.1: per-module const-pool blob laid out at compile time and
/// copied into the arena prefix on every dispatch. Mirrors
/// `relon_codegen_cranelift::codegen::ConstPool` (shape only — the LLVM
/// side keeps it scoped to this crate so the dep direction stays
/// one-way).
///
/// Layout: `[len: u32 LE][utf8 bytes]` records emitted in IR-walk
/// order, aligned to 4. Each `Op::ConstString { idx }` resolves to
/// `string_offsets[idx]` — the byte offset of its record inside
/// [`Self::bytes`] (= the arena-relative offset once the host has
/// copied the blob to the arena prefix).
#[derive(Debug, Default, Clone)]
pub struct ConstPool {
    /// `idx -> byte offset within `bytes`. The emitter materialises
    /// `Op::ConstString { idx }` as `iconst(I32, string_offsets[idx])`.
    pub string_offsets: std::collections::HashMap<u32, u32>,
    /// Materialised bytes in record order. The host trampoline copies
    /// these verbatim to `arena[..bytes.len()]` before every dispatch.
    pub bytes: Vec<u8>,
}

impl ConstPool {
    /// Build the pool by walking every function body in `module` and
    /// collecting each unique `Op::ConstString { idx, value }`. Records
    /// are laid out in walk-order with 4-byte alignment.
    pub fn from_module(module: &IrModule) -> Result<Self, LlvmError> {
        let mut pool = ConstPool::default();
        for func in &module.funcs {
            pool.collect_body(&func.body)?;
        }
        Ok(pool)
    }

    fn collect_body(&mut self, body: &[TaggedOp]) -> Result<(), LlvmError> {
        for tagged in body {
            self.collect_op(&tagged.op)?;
        }
        Ok(())
    }

    fn collect_op(&mut self, op: &Op) -> Result<(), LlvmError> {
        match op {
            Op::ConstString { idx, value } => self.add_string(*idx, value),
            Op::Block { body, .. } | Op::Loop { body, .. } => self.collect_body(body),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                self.collect_body(then_body)?;
                self.collect_body(else_body)
            }
            // Op::Call inlines a bundled-stdlib body whose own
            // `Op::ConstString` literals must also land in the pool —
            // mirror cranelift's recursion through `builtin_stdlib`.
            Op::Call { fn_index, .. } => {
                let stdlib = relon_ir::stdlib::builtin_stdlib();
                if let Some(callee) = stdlib.get(*fn_index as usize) {
                    let body = callee.body_owned();
                    self.collect_body(&body)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn add_string(&mut self, idx: u32, value: &str) -> Result<(), LlvmError> {
        if self.string_offsets.contains_key(&idx) {
            return Ok(());
        }
        // Align to 4 so the `[len: u32]` header lands on a 4-byte
        // boundary — i32 loads through the JIT use `align=4` and we
        // don't want an unaligned trap on hosts where it matters.
        let rem = self.bytes.len() % 4;
        if rem != 0 {
            self.bytes.resize(self.bytes.len() + (4 - rem), 0);
        }
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| LlvmError::Codegen("llvm const pool exceeds u32 range".into()))?;
        let len = u32::try_from(value.len())
            .map_err(|_| LlvmError::Codegen("ConstString length exceeds u32 range".into()))?;
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes.extend_from_slice(value.as_bytes());
        self.string_offsets.insert(idx, off);
        Ok(())
    }
}

/// IR param signature that triggers [`EntryShape::Buffer`]. Mirrors
/// `is_buffer_protocol_signature` on the cranelift side.
pub(crate) fn is_buffer_protocol_signature(params: &[IrType], ret: IrType) -> bool {
    matches!(
        params,
        [
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I64
        ]
    ) && matches!(ret, IrType::I32)
}

/// Phase E.2 multi-function emit: lower every reachable IR function
/// into LLVM. The entry function `entry` is emitted under either the
/// legacy-i64 or buffer-protocol shape; each entry in `helpers` is
/// emitted as a sibling helper function with a plain typed
/// `(params...) -> ret` signature so the entry's `Op::Call` lowering
/// can route to it through a direct LLVM `call` instruction.
///
/// `helper_ir_indices` parallels `helpers`: entry `i` carries the
/// IR-side `funcs` index for the matching helper. Used by the
/// `Op::Call` lowering to resolve `fn_index - stdlib_count` back to the
/// matching `FunctionValue`.
///
/// Phase F.W7 widens the surface to closures-as-values:
///
/// - `lambdas` carries the IR funcs the lowering pass appended to the
///   module's closure table (`#main`-side `fib: (k) => ...` lifts to a
///   lambda Func). Each lambda is declared / emitted with the
///   signature `(state, captures_ptr, ...lambda.params[1..]) -> ret`
///   so the body's `LocalGet(0)` reads the captures_ptr arg, and so
///   `Op::AllocScratch` / `*AtAbsolute` ops inside the body can reach
///   the per-call arena state.
/// - `closure_table` mirrors the IR's `Module::closure_table` so the
///   emitter knows which `fn_table_idx` resolves to which lambda
///   `FunctionValue`. Returned alongside `helper_table` so the
///   `Op::MakeClosure` / `Op::CallClosure` lowering can refer to it.
///
/// `const_pool` ships the per-module ConstString blob the entry +
/// helper bodies index into via `Op::ConstString { idx }`. The host
/// copies `const_pool.bytes` to the arena prefix before every
/// dispatch so the materialised `iconst(I32, offset)` resolves to a
/// stable address.
///
/// Returns the entry `FunctionValue`, the detected entry shape, the
/// helper lookup table the `Emit` driver hands off to the per-function
/// lowering so sibling calls can find their callee, and the closure
/// table (one entry per `fn_table_idx`, in source order).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn emit_module_funcs<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    entry: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helpers: &[&Func],
    helper_ir_indices: Option<&[u32]>,
    lambdas: &[&Func],
    closure_table: &[u32],
) -> Result<
    (
        FunctionValue<'ctx>,
        EntryShape,
        HashMap<u32, FunctionValue<'ctx>>,
        Vec<FunctionValue<'ctx>>,
    ),
    LlvmError,
> {
    // Step 0: declare module-level intrinsics. `llvm.trap` is shared
    // by every Div / Mod sandbox guard so a single declaration covers
    // every per-op guard across every emitted function.
    declare_llvm_trap(ctx, module);

    // Step 1: declare every helper up-front so the entry / sibling
    // bodies can resolve forward references (mutual recursion, the
    // `fib(n - 1) + fib(n - 2)` self-call). LLVM is happy to issue
    // `call @foo` against a declared-only function; the body is
    // attached on the second pass.
    let mut helper_table: HashMap<u32, FunctionValue<'ctx>> = HashMap::new();
    if let Some(ir_indices) = helper_ir_indices {
        if ir_indices.len() != helpers.len() {
            return Err(LlvmError::Codegen(format!(
                "emit_module_funcs: helpers.len()={} but helper_ir_indices.len()={}",
                helpers.len(),
                ir_indices.len()
            )));
        }
    }
    for (i, helper) in helpers.iter().enumerate() {
        let fv = declare_helper_function(ctx, module, helper, i)?;
        let ir_idx = helper_ir_indices.map(|v| v[i]).unwrap_or(i as u32);
        helper_table.insert(ir_idx, fv);
    }

    // Phase F.W7: declare every lambda function up-front. Lambdas use
    // a widened signature `(state, ...lambda.params) -> ret` — the
    // first IR param (already `IrType::I32`, the captures_ptr the IR
    // lowering pass prepended in `lower_closure_as_value`) becomes
    // LLVM param 1 (just past the implicit `*state`). Subsequent
    // user params shift to LLVM param indices 2.. so the body's
    // `LocalGet(idx)` resolves to LLVM param `idx + 1`
    // (`param_base = 1`).
    let mut closure_fn_table: Vec<FunctionValue<'ctx>> = Vec::with_capacity(closure_table.len());
    if lambdas.len() != closure_table.len() {
        return Err(LlvmError::Codegen(format!(
            "emit_module_funcs: lambdas.len()={} but closure_table.len()={}",
            lambdas.len(),
            closure_table.len()
        )));
    }
    for (slot, lambda) in lambdas.iter().enumerate() {
        let fv = declare_lambda_function(ctx, module, lambda, slot)?;
        closure_fn_table.push(fv);
    }

    // Step 2: emit the entry function body.
    let (entry_fn, shape) = if is_buffer_protocol_signature(&entry.params, entry.ret) {
        let fv = emit_buffer_entry_with_helpers_and_closures(
            ctx,
            module,
            entry,
            buffer_return_size,
            const_pool,
            &helper_table,
            &closure_fn_table,
        )?;
        (fv, EntryShape::Buffer)
    } else {
        // The legacy-i64 entry shape covers hand-built fixtures only; it
        // never references ConstString and supplies its own empty pool
        // inside `emit_legacy_entry_impl`.
        let fv = emit_legacy_entry_with_helpers(ctx, module, entry, &helper_table)?;
        (fv, EntryShape::LegacyI64)
    };

    // Step 3: emit each helper body now that every callee is declared.
    for helper in helpers.iter() {
        let helper_fn = helper_table
            .values()
            .find(|fv| {
                // Locate the FunctionValue by name; cheap enough — the
                // helper table is tiny and the find runs once per
                // helper.
                let expected = format!("relon_helper_{}", helper.name);
                fv.get_name().to_string_lossy() == expected
            })
            .copied()
            .ok_or_else(|| {
                LlvmError::Codegen(format!(
                    "emit_module_funcs: helper `{}` declared but FunctionValue missing",
                    helper.name
                ))
            })?;
        emit_helper_body(ctx, module, helper, helper_fn, const_pool, &helper_table)?;
    }

    // Step 4 (Phase F.W7): emit each lambda body. Lambdas share the
    // `helper_table` so the body can route an inner `Op::Call` to a
    // sibling helper (Phase E.2 cross-call). They also share the
    // `closure_fn_table` so a nested `Op::MakeClosure` resolves the
    // matching lambda FunctionValue from its `fn_table_idx`.
    //
    // Build the module-wide self-capture table once before emitting
    // lambda bodies. The table maps each lambda's `fn_table_idx` to
    // the captures-struct offsets that hold self-recursive handles
    // (i.e. handles whose `captures_ptr` field equals the lambda's
    // own captures_ptr arg). The lambda-body emit uses this table to
    // stamp [`Provenance::OwnCaptureHandle`] on the matching capture
    // loads so the recursive call site can pick the direct-call fast
    // path. Empty for modules that have no self-recursive closures.
    let self_capture_table = build_self_capture_table(entry, helpers, lambdas);
    // Devirtualisation (W18): companion table for captures of known
    // (non-self) closures — lets the W18 predicate's `is_prime` call
    // devirtualise inside the predicate lambda body.
    let known_capture_table = build_known_capture_table(entry, helpers, lambdas);
    for (slot, lambda) in lambdas.iter().enumerate() {
        let lambda_fn = closure_fn_table[slot];
        let slot_u32 = slot as u32;
        let offsets = self_capture_table
            .get(&slot_u32)
            .cloned()
            .unwrap_or_default();
        let known_offsets = known_capture_table
            .get(&slot_u32)
            .cloned()
            .unwrap_or_default();
        emit_lambda_body(
            ctx,
            module,
            lambda,
            lambda_fn,
            const_pool,
            &helper_table,
            &closure_fn_table,
            &offsets,
            &known_offsets,
        )?;
    }

    Ok((entry_fn, shape, helper_table, closure_fn_table))
}

/// Phase F.W7 self-recursion fast path: scan every IR function body
/// (entry + helpers + lambdas) for the canonical
/// `Op::MakeClosure { fn_table_idx, captures } ; Op::LetSet { idx, ty:
/// Closure }` pair and collect the captures whose `let_idx` matches the
/// `LetSet`'s `idx` — those are the self-recursive captures stamped by
/// `lower_closure_as_value`'s "let-slot not yet bound" branch.
///
/// Returns `fn_table_idx -> [(capture_offset, self_fn_table_idx)]` so
/// the lambda body emitter can stamp the matching
/// [`Provenance::OwnCaptureHandle`] on each capture load.
///
/// The scan tolerates intervening ops between `MakeClosure` and
/// `LetSet` (none are emitted today; future lowering passes that
/// interleave additional setup ops would still be matched). It bails
/// silently on patterns it can't recognise — the fast path stays
/// opt-in and the slow-path `emit_call_closure` keeps working
/// regardless.
fn build_self_capture_table(
    entry: &Func,
    helpers: &[&Func],
    lambdas: &[&Func],
) -> HashMap<u32, Vec<(u32, u32)>> {
    let mut table: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();

    let scan = |func: &Func, table: &mut HashMap<u32, Vec<(u32, u32)>>| {
        let ops = &func.body;
        for (i, tagged) in ops.iter().enumerate() {
            // Find a MakeClosure immediately followed by a matching
            // `LetSet { ty: Closure }`. The IR lowering pass emits
            // these adjacently (see `lower_anon_dict_body` /
            // `lower_closure_as_value`); intervening ops break the
            // simple match and the slow-path dispatch keeps working.
            let Op::MakeClosure {
                fn_table_idx,
                ref captures,
                ..
            } = tagged.op
            else {
                continue;
            };
            let Some(next) = ops.get(i + 1) else {
                continue;
            };
            let Op::LetSet {
                idx,
                ty: relon_ir::ir::IrType::Closure,
            } = next.op
            else {
                continue;
            };
            for cap in captures {
                if cap.let_idx == idx && matches!(cap.ty, relon_ir::ir::IrType::Closure) {
                    table
                        .entry(fn_table_idx)
                        .or_default()
                        .push((cap.offset, fn_table_idx));
                }
            }
        }
    };

    scan(entry, &mut table);
    for h in helpers {
        scan(h, &mut table);
    }
    for l in lambdas {
        scan(l, &mut table);
    }
    table
}

/// Devirtualisation (W18, 2026-05-30): companion to
/// [`build_self_capture_table`] for *non-self* captures of a closure
/// whose `fn_table_idx` is a compile-time constant.
///
/// Maps each lambda's `fn_table_idx` to the captures-struct offsets that
/// hold a handle produced by a literal `Op::MakeClosure { K }` (a
/// *known* closure), together with that `K`. The lambda-body emit uses
/// this to stamp [`Provenance::KnownClosure`] on the matching capture
/// load (the prologue `LocalGet(0); LoadI32AtAbsolute { offset };
/// LetSet { Closure }`), so a `CallClosure` against the capture (e.g.
/// the W18 predicate's `is_prime(k, 2)` call) emits a direct call
/// instead of the runtime `switch i32 %cc_fn_idx`.
///
/// Soundness: within each function we track, in source order, the
/// most-recent `MakeClosure { K }; LetSet { idx, Closure }` assignment
/// per outer let-slot. Any *other* `LetSet { idx, Closure }` clears the
/// slot — so a let that is reassigned to a dynamically-chosen closure is
/// never recorded as known. A capture is recorded only when its
/// `let_idx` resolves to a still-known slot AND the captured `K` differs
/// from the capturing lambda `L` (a self-capture, `K == L`, is owned by
/// [`build_self_capture_table`], whose `captures_ptr`-reuse fast path is
/// strictly better). The lowering pass emits the capturing
/// `MakeClosure` only after the captured let is bound and reads the live
/// slot, so the tracked `K` is exactly the value the capture holds.
fn build_known_capture_table(
    entry: &Func,
    helpers: &[&Func],
    lambdas: &[&Func],
) -> HashMap<u32, Vec<(u32, u32)>> {
    use relon_ir::ir::IrType as Irt;
    let mut table: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();

    let scan = |func: &Func, table: &mut HashMap<u32, Vec<(u32, u32)>>| {
        let ops = &func.body;
        // outer let-slot -> known captured `fn_table_idx`, last-write
        // wins; cleared when the slot is reassigned a non-known closure.
        let mut known_slots: HashMap<u32, u32> = HashMap::new();
        for (i, tagged) in ops.iter().enumerate() {
            // Maintain `known_slots` off each `LetSet { idx, Closure }`:
            // if the immediately-preceding op is a `MakeClosure { K }`
            // (the canonical `MakeClosure; LetSet` binding the lowering
            // emits) the slot becomes a *known* closure `K`; any other
            // `LetSet { Closure }` stores a value we cannot prove is one
            // statically-known closure, so the slot is dropped. Driving
            // this off the `LetSet` (rather than the `MakeClosure`)
            // avoids the binding `LetSet` clobbering the very entry the
            // preceding `MakeClosure` established.
            if let Op::LetSet {
                idx,
                ty: Irt::Closure,
            } = tagged.op
            {
                if let Some(Op::MakeClosure { fn_table_idx, .. }) =
                    i.checked_sub(1).and_then(|p| ops.get(p)).map(|t| &t.op)
                {
                    known_slots.insert(idx, *fn_table_idx);
                } else {
                    known_slots.remove(&idx);
                }
                continue;
            }
            // At a capturing `MakeClosure { L }`, record each capture
            // that reads a still-known slot. The capturing closure's own
            // handle need NOT be stored to a let — the W18 predicate is
            // passed straight into `_list_filter` — because the fact
            // recorded here is about lambda `L`'s captures-struct layout
            // (offset O holds known closure K), which is fixed by `L`'s
            // own `MakeClosure` captures and the known-ness of the
            // captured outer let, independent of where `L`'s handle goes.
            if let Op::MakeClosure {
                fn_table_idx: l_idx,
                ref captures,
                ..
            } = tagged.op
            {
                for cap in captures {
                    if !matches!(cap.ty, Irt::Closure) {
                        continue;
                    }
                    if let Some(&k_idx) = known_slots.get(&cap.let_idx) {
                        // `k_idx == l_idx` is a self-capture — owned by
                        // `build_self_capture_table`; skip here.
                        if k_idx != l_idx {
                            table.entry(l_idx).or_default().push((cap.offset, k_idx));
                        }
                    }
                }
            }
        }
    };

    scan(entry, &mut table);
    for h in helpers {
        scan(h, &mut table);
    }
    for l in lambdas {
        scan(l, &mut table);
    }
    table
}

/// Devirtualisation (W18) correctness helper: collect every let-slot
/// index that a body assigns via `Op::LetSet { ty: Closure }`, recursing
/// into nested `Op::If` / `Op::Block` / `Op::Loop` bodies. Used by
/// `emit_loop` to conservatively invalidate the `KnownClosure` let-slot
/// tracker for any closure slot the loop body reassigns, so a
/// cross-iteration read cannot devirtualise to a stale target.
fn collect_closure_letset_slots(body: &[TaggedOp], out: &mut Vec<u32>) {
    for t in body {
        match &t.op {
            Op::LetSet {
                idx,
                ty: relon_ir::ir::IrType::Closure,
            } => out.push(*idx),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect_closure_letset_slots(then_body, out);
                collect_closure_letset_slots(else_body, out);
            }
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                collect_closure_letset_slots(body, out);
            }
            _ => {}
        }
    }
}

/// Declare a sibling helper function's LLVM signature without emitting
/// its body. Used to seat every helper into the module so the entry's
/// `Op::Call` lowering can resolve forward references (recursion,
/// mutual recursion). Sibling helpers use a plain typed
/// `(params...) -> ret` shape — no `*state` pointer, no buffer
/// protocol; the test harness drives recursive Int-only functions
/// directly. When the IR layer grows first-class closure values
/// (Phase F), this signature widens to carry `(*state, captures, ...)`.
fn declare_helper_function<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    slot: usize,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    let mut param_types: Vec<BasicMetadataTypeEnum<'ctx>> = Vec::with_capacity(func.params.len());
    for (i, p) in func.params.iter().enumerate() {
        let bt = ir_ty_to_llvm_abi(ctx, *p).ok_or_else(|| {
            LlvmError::UnsupportedSignature(format!(
                "llvm-aot: helper `{}` param #{i} type {p:?} unsupported",
                func.name
            ))
        })?;
        param_types.push(basic_to_metadata(bt));
    }
    let ret_bt = ir_ty_to_llvm_abi(ctx, func.ret).ok_or_else(|| {
        LlvmError::UnsupportedSignature(format!(
            "llvm-aot: helper `{}` return type {:?} unsupported",
            func.name, func.ret
        ))
    })?;
    let fn_type = match ret_bt {
        BasicTypeEnum::IntType(t) => t.fn_type(&param_types, false),
        BasicTypeEnum::FloatType(t) => t.fn_type(&param_types, false),
        BasicTypeEnum::PointerType(t) => t.fn_type(&param_types, false),
        other => {
            return Err(LlvmError::Codegen(format!(
                "llvm-aot: helper `{}` ret BasicType {other:?} unsupported",
                func.name
            )));
        }
    };
    // Use a deterministic LLVM symbol so the entry's call site can be
    // pretty-printed in the IR dump. The slot keeps multiple helpers
    // with the same source name (shouldn't happen, but cheap) from
    // colliding.
    let _ = slot;
    let llvm_name = format!("relon_helper_{}", func.name);
    let fv = module.add_function(&llvm_name, fn_type, Some(Linkage::Internal));
    Ok(fv)
}

/// Phase F.W7: declare a lambda function's LLVM signature without
/// emitting its body. Lambdas always carry the
/// `(state: ptr, ...lambda.params) -> ret` signature — the first IR
/// param is the captures_ptr the IR lowering pass prepended in
/// `lower_closure_as_value`, surfaced through LLVM param 1. Subsequent
/// LLVM params correspond to the lambda's user-visible args.
///
/// The implicit `*state` pointer at LLVM param 0 mirrors the
/// buffer-protocol entry's leading state slot so the lambda body's
/// `Op::AllocScratch{,Dyn}` / `Op::*AtAbsolute` ops can resolve
/// `arena_base` + scratch cursors through the same helper paths the
/// entry uses.
fn declare_lambda_function<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    slot: usize,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    let ptr_t = ctx.ptr_type(AddressSpace::default());
    let mut param_types: Vec<BasicMetadataTypeEnum<'ctx>> =
        Vec::with_capacity(1 + func.params.len());
    param_types.push(ptr_t.into());
    for (i, p) in func.params.iter().enumerate() {
        let bt = ir_ty_to_llvm_abi(ctx, *p).ok_or_else(|| {
            LlvmError::UnsupportedSignature(format!(
                "llvm-aot: lambda `{}` param #{i} type {p:?} unsupported",
                func.name
            ))
        })?;
        param_types.push(basic_to_metadata(bt));
    }
    let ret_bt = ir_ty_to_llvm_abi(ctx, func.ret).ok_or_else(|| {
        LlvmError::UnsupportedSignature(format!(
            "llvm-aot: lambda `{}` return type {:?} unsupported",
            func.name, func.ret
        ))
    })?;
    let fn_type = match ret_bt {
        BasicTypeEnum::IntType(t) => t.fn_type(&param_types, false),
        BasicTypeEnum::FloatType(t) => t.fn_type(&param_types, false),
        BasicTypeEnum::PointerType(t) => t.fn_type(&param_types, false),
        other => {
            return Err(LlvmError::Codegen(format!(
                "llvm-aot: lambda `{}` ret BasicType {other:?} unsupported",
                func.name
            )));
        }
    };
    // `relon_lambda_<slot>_<name>` so the emitted IR dump is greppable
    // when debugging which `fn_table_idx` mapped to which body.
    let llvm_name = format!("relon_lambda_{}_{}", slot, func.name);
    let fv = module.add_function(&llvm_name, fn_type, Some(Linkage::Internal));
    Ok(fv)
}

/// Phase E.2: declare the `llvm.trap` intrinsic on `module` if it is
/// not already present. The intrinsic has signature `void @llvm.trap()`
/// — calling it raises a target-specific trap (a `ud2` on x86-64) that
/// the host's `panic` handler can catch when paired with an
/// `unreachable`. Cheap to call on every emit pass; we keep the lookup
/// idempotent so test fixtures that re-enter the emitter don't end up
/// with duplicate declarations.
fn declare_llvm_trap<'ctx>(ctx: &'ctx Context, module: &LlvmModule<'ctx>) -> FunctionValue<'ctx> {
    if let Some(f) = module.get_function("llvm.trap") {
        return f;
    }
    let void_t = ctx.void_type();
    let fn_ty = void_t.fn_type(&[], false);
    module.add_function("llvm.trap", fn_ty, None)
}

/// #359 (W20): map an [`IrType`] to the LLVM type used in a helper /
/// lambda **call ABI** slot. This mirrors the operand-stack
/// convention where `F64` rides as its 64-bit *bit pattern* in an i64
/// register: `F64` maps to `i64`, not `double`. Keeping the ABI int-
/// only means a `CallClosure` / `Op::Call` site never has to bitcast
/// between the i64-bits stack representation and a native-float
/// argument / return slot — the value flows through verbatim. The
/// W20 n-body helpers (`pair_force` / `accel` return `F64`,
/// `pair_force` takes an `F64` mass) are the first closures with a
/// Float in their signature; without this they'd declare a `double`
/// slot that the i64-bits operand stack cannot feed.
fn ir_ty_to_llvm_abi<'ctx>(ctx: &'ctx Context, ty: IrType) -> Option<BasicTypeEnum<'ctx>> {
    match ty {
        IrType::I64 | IrType::F64 => Some(ctx.i64_type().into()),
        IrType::I32 | IrType::Bool | IrType::Null => Some(ctx.i32_type().into()),
        IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => Some(ctx.i32_type().into()),
    }
}

fn basic_to_metadata(bt: BasicTypeEnum<'_>) -> BasicMetadataTypeEnum<'_> {
    match bt {
        BasicTypeEnum::IntType(t) => t.into(),
        BasicTypeEnum::FloatType(t) => t.into(),
        BasicTypeEnum::PointerType(t) => t.into(),
        BasicTypeEnum::ArrayType(t) => t.into(),
        BasicTypeEnum::StructType(t) => t.into(),
        BasicTypeEnum::VectorType(t) => t.into(),
        BasicTypeEnum::ScalableVectorType(t) => t.into(),
    }
}

/// Lower a sibling helper's body against its declared LLVM
/// `FunctionValue`. Mirrors [`emit_legacy_entry`] but without enforcing
/// the legacy-i64 envelope — helpers may carry any
/// [`IrType`]-shaped param / return mix that `ir_ty_to_llvm_abi`
/// accepts.
fn emit_helper_body<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    llvm_fn: FunctionValue<'ctx>,
    const_pool: &ConstPool,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
) -> Result<(), LlvmError> {
    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        /*arena_base_ptr=*/ None,
        /*state_ptr=*/ None,
        /*buffer_return_size=*/ 0,
        const_pool,
    );
    // Helper functions have no implicit state slot; `LocalGet(0)` maps
    // straight to LLVM param 0.
    emit.param_base = 0;
    emit.helper_table = Some(helper_table.clone());
    // Record the IR-declared return type so `Op::Return` knows what to
    // widen / truncate to when the operand stack value's width differs
    // from the LLVM signature's return slot.
    emit.helper_ret_ty = Some(func.ret);
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;
    Ok(())
}

/// Phase F.W7: emit a lambda body. Mirrors [`emit_helper_body`] but:
///
/// - The first LLVM param (`*state`) is materialised into
///   `arena_base_ptr` + `state_ptr` so the body's
///   `Op::AllocScratch{,Dyn}` / `Op::*AtAbsolute` ops resolve against
///   the per-call arena state. Required because lambdas read captures
///   via `LocalGet(0); LoadI32AtAbsolute { offset }` against the
///   captures struct in scratch.
/// - `param_base = 1` so the IR's `LocalGet(idx)` skips the implicit
///   state slot — `LocalGet(0)` therefore reads the captures_ptr at
///   LLVM param 1, matching what the IR lowering pass laid out in
///   `lower_closure_as_value`.
/// - The closure table is threaded through so nested
///   `Op::MakeClosure` / `Op::CallClosure` ops inside the lambda body
///   keep resolving against the same module-wide lambda set the entry
///   uses.
#[allow(clippy::too_many_arguments)]
fn emit_lambda_body<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    llvm_fn: FunctionValue<'ctx>,
    const_pool: &ConstPool,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
    closure_fn_table: &[FunctionValue<'ctx>],
    self_capture_offsets: &[(u32, u32)],
    known_capture_offsets: &[(u32, u32)],
) -> Result<(), LlvmError> {
    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Materialise `state_ptr` + `arena_base_ptr` at function entry.
    // Same pointer-arithmetic shape the buffer entry uses — the lambda
    // shares the per-call `ArenaState` layout because the host (the
    // entry function or another lambda) passes its own state pointer
    // through to the call indirect site verbatim.
    let i32_t = ctx.i32_type();
    let i64_t = ctx.i64_type();
    let i8_t = ctx.i8_type();
    let ptr_t = ctx.ptr_type(AddressSpace::default());
    let state_param = llvm_fn
        .get_nth_param(0)
        .ok_or_else(|| LlvmError::Codegen(format!("lambda `{}` missing state param", func.name)))?
        .into_pointer_value();
    let arena_base_gep = unsafe {
        builder
            .build_in_bounds_gep(
                i8_t,
                state_param,
                &[i32_t.const_int(ARENA_STATE_OFFSET_BASE as u64, false)],
                "lambda_arena_base_gep",
            )
            .map_err(|e| LlvmError::Codegen(format!("lambda arena_base GEP: {e}")))?
    };
    // TODO(P3-wasm32): use DataLayout pointer width instead of i64
    // for the arena-base word load + inttoptr below.
    let arena_base_int = builder
        .build_load(i64_t, arena_base_gep, "lambda_arena_base")
        .map_err(|e| LlvmError::Codegen(format!("lambda arena_base load: {e}")))?
        .into_int_value();
    let arena_base_ptr = builder
        .build_int_to_ptr(arena_base_int, ptr_t, "lambda_arena_base_ptr")
        .map_err(|e| LlvmError::Codegen(format!("lambda arena_base inttoptr: {e}")))?;

    // Stash the captures_ptr LLVM param (param 1) so the self-recursion
    // fast path in `emit_call_closure` can reuse it directly instead
    // of round-tripping through a `captures_ptr` field load on every
    // recursion. The lambda signature pins this to LLVM param 1 (param
    // 0 is `*state`) — see `declare_lambda_function`.
    let captures_ptr_param = llvm_fn
        .get_nth_param(1)
        .ok_or_else(|| {
            LlvmError::Codegen(format!("lambda `{}` missing captures_ptr param", func.name))
        })?
        .into_int_value();

    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        Some(arena_base_ptr),
        Some(state_param),
        /*buffer_return_size=*/ 0,
        const_pool,
    );
    // LLVM param 0 is `*state`; the IR's params (including the
    // implicit captures_ptr at IR index 0) start at LLVM param 1.
    emit.param_base = 1;
    emit.helper_table = Some(helper_table.clone());
    emit.closure_fn_table = closure_fn_table.to_vec();
    // The lambda body's `Op::Return` carries the IR-declared return
    // type so the dispatcher knows what LLVM `ret` shape to emit.
    emit.helper_ret_ty = Some(func.ret);
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.self_capture_offsets = self_capture_offsets.to_vec();
    emit.known_capture_offsets = known_capture_offsets.to_vec();
    emit.captures_ptr_param = Some(captures_ptr_param);
    emit.lower_body(&func.body)?;
    Ok(())
}

/// Phase D.1: emit a typed `(i64, i64, ...) -> i64` fast entry
/// alongside the buffer-protocol entry. Reuses the IR body's op
/// stream but rewrites every buffer-protocol `LoadField` into a
/// direct LLVM param read (via `profile.arg_offsets`) and every
/// trailing `StoreField` at the return-value offset into a `ret`
/// against the stashed value.
///
/// Returns `Err` when the IR contains ops outside the fast-path
/// envelope (string ops, sandbox traps, pointer-indirect StoreField,
/// stdlib calls — anything that escapes the simple Int-arithmetic
/// loop). The evaluator side surfaces this as "fast path unavailable;
/// fall back to the buffer entry" rather than a hard error so adding
/// more workloads doesn't risk regressing the buffer path.
pub(crate) fn emit_fast_entry<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    profile: &FastPathProfile,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
    closure_fn_table: &[FunctionValue<'ctx>],
) -> Result<FunctionValue<'ctx>, LlvmError> {
    if !is_buffer_protocol_signature(&func.params, func.ret) {
        return Err(LlvmError::UnsupportedSignature(
            "fast-path entry requires buffer-protocol IR".into(),
        ));
    }
    let arity = profile.arg_offsets.len();
    if arity > 8 {
        // Cap at 8 to keep the typed dispatch table in evaluator.rs
        // finite. Sources with arity > 8 stay on the buffer path —
        // their boundary cost is amortised across more work anyway.
        return Err(LlvmError::UnsupportedSignature(format!(
            "fast-path entry: arity {arity} exceeds cap of 8"
        )));
    }

    let i64_t = ctx.i64_type();
    let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = (0..arity).map(|_| i64_t.into()).collect();
    let fn_type = i64_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL_FAST, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "fast_entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Reserve an alloca for the return value. The fast emitter
    // rewrites the trailing `StoreField` / `StoreFieldAtRecord` at
    // the return slot (which under buffer protocol writes the i64
    // result into the arena) to a store into this slot; the implicit
    // `Op::Return` at end-of-body loads from the slot and `ret`s it.
    // Placing the alloca in the entry block lets LLVM's mem2reg
    // promote it to SSA across the loop boundary.
    let ret_slot = builder
        .build_alloca(i64_t, "fast_ret_slot")
        .map_err(|e| LlvmError::Codegen(format!("fast ret_slot alloca: {e}")))?;
    // Initialise to 0 so any early `Op::Return` (no value path) still
    // produces a defined value — matches the buffer entry's
    // "ret root_size when no scalar stored" envelope.
    builder
        .build_store(ret_slot, i64_t.const_zero())
        .map_err(|e| LlvmError::Codegen(format!("fast ret_slot init: {e}")))?;

    // The fast entry is a typed `(i64...) -> i64` shape derived from
    // the buffer-protocol IR after the dispatch-boundary rewrite. It
    // doesn't touch the const-data pool (the IR only contains scalar
    // arithmetic ops) so we hand it an empty pool to keep
    // `Emit::new` polymorphic.
    let empty_pool = ConstPool::default();
    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        /*arena_base_ptr=*/ None,
        /*state_ptr=*/ None,
        /*buffer_return_size=*/ 0,
        &empty_pool,
    );
    emit.fast_path = Some(FastEmit {
        profile: profile.clone(),
        ret_slot,
    });
    // LLVM param i corresponds to arg i — no implicit state slot for
    // the fast entry. `LocalGet` should never appear in the body
    // because the IR producer only emits LocalGet for the handshake
    // params (which the fast path doesn't pass).
    emit.param_base = 0;
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    // Phase D.2: plumb the module-wide helper and closure tables so
    // an in-body `Op::Call` / `Op::MakeClosure` / `Op::CallClosure`
    // can resolve sibling functions. The fast emitter's per-op rewrites
    // (`MakeClosure` → virtualised closure, `CallClosure` → direct
    // call with null state/captures) consult these tables to pick the
    // matching `FunctionValue`.
    emit.helper_table = Some(helper_table.clone());
    emit.closure_fn_table = closure_fn_table.to_vec();
    emit.lower_body(&func.body)?;

    // The buffer-protocol IR ends with `Op::Return` which the fast
    // emitter rewrote into a load+ret. If the body fell through
    // without an explicit Return (shouldn't happen for well-formed
    // `#main` IR, but be defensive), seal it with a load+ret.
    if let Some(cur) = builder.get_insert_block() {
        if cur.get_terminator().is_none() {
            let v = builder
                .build_load(i64_t, ret_slot, "fast_ret_load")
                .map_err(|e| LlvmError::Codegen(format!("fast trailing load: {e}")))?
                .into_int_value();
            builder
                .build_return(Some(&v))
                .map_err(|e| LlvmError::Codegen(format!("fast trailing ret: {e}")))?;
        }
    }

    Ok(llvm_fn)
}

// ---------------------------------------------------------------------------
// Legacy-i64 entry (Phase A bootstrap envelope, retained for tests)
// ---------------------------------------------------------------------------

fn emit_legacy_entry_with_helpers<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    emit_legacy_entry_impl(ctx, module, func, Some(helper_table))
}

/// Emit a Phase-A `(I64...) -> I64` function. Used by tests + the
/// Phase A bootstrap benchmarks that exercise the hand-built IR
/// fixtures directly (no buffer-protocol wrapping).
fn emit_legacy_entry_impl<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    helper_table: Option<&HashMap<u32, FunctionValue<'ctx>>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    for (i, p) in func.params.iter().enumerate() {
        if *p != IrType::I64 {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot: legacy-i64 envelope expects I64 param at #{i}, got {p:?}"
            )));
        }
    }
    if func.ret != IrType::I64 {
        return Err(LlvmError::UnsupportedSignature(format!(
            "llvm-aot: legacy-i64 envelope expects I64 return, got {:?}",
            func.ret
        )));
    }

    let i64_t = ctx.i64_type();
    let param_types: Vec<BasicMetadataTypeEnum<'ctx>> =
        (0..func.params.len()).map(|_| i64_t.into()).collect();
    let fn_type = i64_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Legacy-i64 entry shape only consumes the hand-built fixtures
    // (helloworld_arith) which never reference ConstString — an empty
    // pool is enough.
    let empty_pool = ConstPool::default();
    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::LegacyI64,
        None,
        None,
        /*buffer_return_size=*/ 0,
        &empty_pool,
    );
    // Param order under the legacy envelope: every IR LocalGet(i)
    // maps to llvm_fn.param(i) — no implicit state slot.
    emit.param_base = 0;
    if let Some(table) = helper_table {
        emit.helper_table = Some(table.clone());
    }
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;

    Ok(llvm_fn)
}

// ---------------------------------------------------------------------------
// Buffer-protocol entry (Phase B production envelope)
// ---------------------------------------------------------------------------

// Retained for symmetry with `emit_legacy_entry_with_helpers`; the
// Phase F.W7 emit path always routes through
// `emit_buffer_entry_with_helpers_and_closures` so a closure-free
// module still gets the new entry shape (with an empty closure
// table). Marked `#[allow(dead_code)]` to keep the symmetric pair
// visible without firing the unused-function lint.
#[allow(dead_code)]
fn emit_buffer_entry_with_helpers<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    emit_buffer_entry_impl(
        ctx,
        module,
        func,
        buffer_return_size,
        const_pool,
        Some(helper_table),
        &[],
    )
}

/// Phase F.W7 variant: same as [`emit_buffer_entry_with_helpers`] but
/// also threads the closure function-pointer table into the entry's
/// `Emit` so the body's `Op::MakeClosure` lowering can stamp the
/// matching `fn_table_idx` into the closure handle.
fn emit_buffer_entry_with_helpers_and_closures<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helper_table: &HashMap<u32, FunctionValue<'ctx>>,
    closure_fn_table: &[FunctionValue<'ctx>],
) -> Result<FunctionValue<'ctx>, LlvmError> {
    emit_buffer_entry_impl(
        ctx,
        module,
        func,
        buffer_return_size,
        const_pool,
        Some(helper_table),
        closure_fn_table,
    )
}

/// Emit the buffer-protocol entry function. The cranelift backend's
/// equivalent lives in `relon-codegen-cranelift::codegen::mod.rs` —
/// signature mirrored here so a host that holds either evaluator
/// can dispatch through the same `(state, in_ptr, …)` argv shape.
fn emit_buffer_entry_impl<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
    buffer_return_size: u32,
    const_pool: &ConstPool,
    helper_table: Option<&HashMap<u32, FunctionValue<'ctx>>>,
    closure_fn_table: &[FunctionValue<'ctx>],
) -> Result<FunctionValue<'ctx>, LlvmError> {
    let i32_t = ctx.i32_type();
    let i64_t = ctx.i64_type();
    let ptr_t = ctx.ptr_type(AddressSpace::default());

    // (*state, i32 in_ptr, i32 in_len, i32 out_ptr, i32 out_cap, i64 caps) -> i32
    let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = vec![
        ptr_t.into(),
        i32_t.into(),
        i32_t.into(),
        i32_t.into(),
        i32_t.into(),
        i64_t.into(),
    ];
    let fn_type = i32_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    // Resolve the per-call arena base once at function entry. The
    // LoadField / StoreField helpers consume this cached value so
    // the JIT doesn't reload `state->arena_base` on every access.
    let state_param = llvm_fn
        .get_nth_param(0)
        .ok_or_else(|| LlvmError::Codegen("buffer entry missing state param".into()))?
        .into_pointer_value();

    // Pointer arithmetic on the state struct: GEP by ARENA_STATE_OFFSET_BASE
    // bytes through an i8 view, then load the `usize` arena base.
    // We use opaque pointers so the GEP element type only matters
    // for the offset calculation.
    let i8_t = ctx.i8_type();
    let arena_base_gep = unsafe {
        builder
            .build_in_bounds_gep(
                i8_t,
                state_param,
                &[i32_t.const_int(ARENA_STATE_OFFSET_BASE as u64, false)],
                "arena_base_gep",
            )
            .map_err(|e| LlvmError::Codegen(format!("arena_base GEP: {e}")))?
    };
    // `arena_base` is `usize`. On every supported host that's i64
    // (we only target x86_64 today; the inkwell feature set in the
    // Cargo.toml is `target-x86`). If we add a 32-bit host the
    // load type needs to follow `pointer_type` width — Phase B
    // assumes the workspace's only target is 64-bit.
    // TODO(P3-wasm32): use DataLayout pointer width instead of i64
    // for the arena-base word load + inttoptr below.
    let arena_base_int = builder
        .build_load(i64_t, arena_base_gep, "arena_base")
        .map_err(|e| LlvmError::Codegen(format!("arena_base load: {e}")))?
        .into_int_value();
    let arena_base_ptr = builder
        .build_int_to_ptr(arena_base_int, ptr_t, "arena_base_ptr")
        .map_err(|e| LlvmError::Codegen(format!("arena_base inttoptr: {e}")))?;

    // Phase E.1 prologue: init `state.tail_cursor = buffer_return_size`
    // so the first pointer-indirect StoreField lands past the fixed
    // area. Cheap (one store per call) — keeping it unconditional
    // avoids a body pre-scan. Bodies that never touch the tail
    // cursor pay the dead store; mem2reg / DSE eliminate it at -O3.
    let tail_init_gep = unsafe {
        builder
            .build_in_bounds_gep(
                i8_t,
                state_param,
                &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                "tail_cursor_init_gep",
            )
            .map_err(|e| LlvmError::Codegen(format!("tail_cursor init GEP: {e}")))?
    };
    let tail_init = i32_t.const_int(u64::from(buffer_return_size), false);
    builder
        .build_store(tail_init_gep, tail_init)
        .map_err(|e| LlvmError::Codegen(format!("tail_cursor init store: {e}")))?;

    let mut emit = Emit::new(
        ctx,
        &builder,
        module,
        llvm_fn,
        EntryShape::Buffer,
        Some(arena_base_ptr),
        Some(state_param),
        buffer_return_size,
        const_pool,
    );
    // Buffer-protocol LocalGet(0..=3) reads the four i32 handshake
    // slots; LocalGet(4) reads the i64 `caps` slot. The state
    // pointer occupies slot 0 in the LLVM function — IR locals
    // start at +1 from there.
    emit.param_base = 1;
    if let Some(table) = helper_table {
        emit.helper_table = Some(table.clone());
    }
    emit.closure_fn_table = closure_fn_table.to_vec();
    emit.llvm_trap_fn = Some(declare_llvm_trap(ctx, module));
    emit.lower_body(&func.body)?;

    Ok(llvm_fn)
}

// ---------------------------------------------------------------------------
// Per-function emitter state
// ---------------------------------------------------------------------------

/// Per-function emitter state. Holds the inkwell builder borrow,
/// the LLVM function the emit targets, the IR's operand stack, and
/// the alloca slots backing `LetSet` / `LetGet`.
///
/// `param_base` accounts for the entry-shape's implicit param slot:
/// the buffer-protocol entry has the `*state` pointer at LLVM param
/// 0, so `LocalGet(0)` resolves to LLVM param 1. The legacy-i64
/// entry has no implicit slot, so `param_base = 0`.
pub(crate) struct Emit<'ctx, 'b, 'cp> {
    pub(crate) ctx: &'ctx Context,
    pub(crate) builder: &'b Builder<'ctx>,
    pub(crate) func: FunctionValue<'ctx>,
    /// Phase F.1: cached module reference so per-op lowering can
    /// declare extern symbols (the F.1 `str.contains` host shim) on
    /// demand without threading the module through every helper. The
    /// reference is borrowed for the emit pass only; `inkwell` keeps
    /// `Module` and `FunctionValue` lifetimes orthogonal so a borrow
    /// here doesn't conflict with the surrounding `add_function`
    /// calls in the entry/helper emit paths.
    pub(crate) module: &'b LlvmModule<'ctx>,
    pub(crate) shape: EntryShape,
    /// Cached `arena_base` pointer for the buffer-protocol entry.
    /// `None` for the legacy entry shape — `LoadField` / `StoreField`
    /// reject themselves before reaching for this value.
    pub(crate) arena_base_ptr: Option<PointerValue<'ctx>>,
    /// Cached state-pointer LLVM value (param 0 of the buffer entry).
    /// Phase E.1 uses it to load / store the per-call tail-cursor /
    /// scratch-cursor / scratch-base slots. `None` outside the
    /// buffer-protocol entry shape.
    pub(crate) state_ptr: Option<PointerValue<'ctx>>,
    /// Operand stack mirroring the IR's virtual stack. Every value
    /// in flight is an LLVM integer of the matching IR type. The
    /// pair tags the IR type so consumers can pick the right
    /// signed / unsigned predicate without re-deriving it.
    pub(crate) stack: Vec<TypedValue<'ctx>>,
    /// `LetSet { idx }` alloca slots, keyed by `(idx, ty)`. Each
    /// idx has at most one type at a time — the IR lowering pass
    /// guarantees no aliasing between idx's of different types.
    pub(crate) let_slots: std::collections::HashMap<u32, (PointerValue<'ctx>, IrType)>,
    /// LLVM param offset corresponding to `LocalGet(0)`. See
    /// [`Self::lookup_param`] — `param_base + idx` is the LLVM
    /// param index.
    pub(crate) param_base: u32,
    /// Label stack carrying the (entry_bb, exit_bb, kind) of every
    /// nested [`Op::Block`] / [`Op::Loop`]. `Br { label_depth }`
    /// indexes from the back (depth 0 = innermost). `Block`s exit
    /// to their tail; `Loop`s exit to their head.
    pub(crate) label_stack: Vec<LabelFrame<'ctx>>,
    /// Monotonic counter to mint unique LLVM basic block / value
    /// names so the dumped IR is human-readable.
    pub(crate) name_seq: u32,
    /// Phase B: hard-coded `return_root_size` returned from a
    /// buffer-protocol `Op::Return`. The IR producer leaves no
    /// value on the operand stack for `Return` under buffer
    /// protocol — the trampoline reads back `bytes_written` to
    /// decode the output record. We hard-code this to the schema's
    /// `return_layout.root_size`, passed in at emit time.
    pub(crate) buffer_return_size: u32,
    /// Phase D.1: set when emitting the fast-path entry. The
    /// `Op::LoadField` / `Op::StoreField` / `Op::Return` lowering
    /// branches consult this to rewrite the buffer-protocol IR
    /// against the typed `(i64...) -> i64` LLVM signature.
    pub(crate) fast_path: Option<FastEmit<'ctx>>,
    /// Phase E.2 multi-function lookup: when populated, `Op::Call`
    /// with `fn_index >= stdlib_function_count()` resolves to the
    /// matching sibling `FunctionValue` and emits a direct LLVM
    /// `call`. The map is keyed by IR-side `funcs` index (i.e.
    /// `fn_index - stdlib_count`). Empty for hand-built fixtures that
    /// never reference user-defined functions.
    pub(crate) helper_table: Option<HashMap<u32, FunctionValue<'ctx>>>,
    /// Phase E.2: when emitting a helper body (not the entry), this
    /// carries the IR-declared return type so `Op::Return` can pick
    /// the right LLVM `ret` shape. `None` while lowering the entry
    /// body — the entry's return shape is dictated by `EntryShape`.
    pub(crate) helper_ret_ty: Option<IrType>,
    /// Phase E.2: cached `llvm.trap` intrinsic `FunctionValue`. The
    /// intrinsic is declared once per module (in
    /// [`emit_module_funcs`]); each `Emit` snapshots the pointer so
    /// per-op `Div(I64)` / `Mod(I64)` guards can call it without
    /// re-querying the module.
    pub(crate) llvm_trap_fn: Option<FunctionValue<'ctx>>,
    /// Phase E.1: per-module const-data lookup. `Op::ConstString { idx }`
    /// reads the matching offset and pushes `iconst(I32, off)`.
    pub(crate) const_pool: &'cp ConstPool,
    /// Phase E.1: stack of inline call frames. `Op::Call` pushes one
    /// before lowering the callee body; `Op::Return` inside the
    /// callee body pops the typed value into the topmost frame's
    /// result alloca and jumps to its exit block. The callee's
    /// `LocalGet(idx)` resolves to `params[idx]` rather than the
    /// entry's LLVM params; `LetGet/LetSet` indices are remapped
    /// against `let_offset` so concurrent inline frames don't clash.
    pub(crate) inline_frames: Vec<InlineFrame<'ctx>>,
    /// Phase E.1: did the body emit a pointer-indirect StoreField?
    /// When set, the buffer-protocol epilogue returns the post-bump
    /// tail cursor (in bytes past `out_ptr`) rather than the
    /// statically-known `buffer_return_size`. Mirrors cranelift's
    /// `needs_tail_cursor` flag.
    pub(crate) needs_tail_cursor: bool,
    /// Phase F.W7: ordered list of lambda `FunctionValue`s, indexed by
    /// `fn_table_idx`. `Op::MakeClosure { fn_table_idx }` stamps the
    /// matching index into the closure handle's `fn_table_idx` slot
    /// and uses the same lookup to resolve the function pointer to
    /// stash. `Op::CallClosure` reads the handle's `fn_table_idx`
    /// slot and dispatches indirectly through a private global table
    /// of function pointers seeded from this list. Empty when the
    /// module contains no lambdas.
    pub(crate) closure_fn_table: Vec<FunctionValue<'ctx>>,
    /// Phase F.W7: per-IR-`record_local_idx` allocas backing
    /// `Op::AllocRootRecord` / `Op::StoreFieldAtRecord`. The slot
    /// holds an i32 out_ptr-relative offset; `AllocRootRecord` writes
    /// `0` there (root sits at `out_ptr + 0`), `StoreFieldAtRecord`
    /// reads it back to compute the destination address. Mirrors
    /// cranelift's `record_locals` map.
    pub(crate) record_locals: std::collections::HashMap<u32, PointerValue<'ctx>>,
    /// Phase H: bytes literal pushed by the *immediately preceding*
    /// `Op::ConstString` op (i.e. still the top-of-stack at the start
    /// of the next `lower_op` call). Cleared at the start of every
    /// `lower_op` and re-populated by the `Op::ConstString` arm at
    /// its tail. The `Op::Call` arm reads this when `fn_index ==
    /// STDLIB_IDX_CONTAINS` to detect the const-needle case and
    /// inline a tight byte-scan loop, skipping the
    /// `relon_llvm_str_contains_arena` extern shim's FFI boundary
    /// (~10-15 cycles of prologue/epilogue per call on x86_64). On
    /// the W4 / W4_long hot loops the needle is always a
    /// compile-time const (`"x"`), so the const-needle fast path
    /// fires 100% of iters. Stays `None` when the needle came in via
    /// `LocalGet` / `LetGet` / any non-`ConstString` producer — those
    /// fall through to the existing extern path.
    pub(crate) last_const_string: Option<Vec<u8>>,
    /// Phase F.W7 self-recursion fast path: per-lambda map of captures
    /// struct offsets that hold a self-recursive closure handle, keyed
    /// by the `fn_table_idx` of the enclosing lambda. Populated only
    /// for lambda bodies (the entry / helpers leave it empty); the
    /// scanner in `build_self_capture_table` correlates each
    /// `Op::MakeClosure` in the entry with the immediately following
    /// `LetSet { idx, ty: Closure }` to identify captures whose
    /// `cap.let_idx == idx` (i.e. the binding being assigned right
    /// after MakeClosure — the canonical IR shape for a self-recursive
    /// closure-as-value let). The value `Vec<(offset,
    /// self_fn_table_idx)>` lets the lambda-prologue `Op::LocalGet(0);
    /// Op::LoadI32AtAbsolute { offset }` chain stamp the matching
    /// [`Provenance::OwnCaptureHandle`] on the produced handle so the
    /// downstream `Op::CallClosure` can pick the direct-call fast path
    /// (skip handle deref, skip switch, reuse the lambda's own
    /// captures_ptr LLVM param 1). Empty when the lambda has no
    /// self-recursive captures or when self-recursion detection is
    /// unavailable (legacy / fixture entries that bypass the
    /// MakeClosure → LetSet pattern).
    pub(crate) self_capture_offsets: Vec<(u32, u32)>,
    /// Phase F.W7 self-recursion fast path: let-slot indices that hold
    /// a self-recursive closure handle along with the enclosing
    /// lambda's `fn_table_idx`. Populated by `Op::LetSet` when the
    /// stored value carries [`Provenance::OwnCaptureHandle`] so the
    /// matching `Op::LetGet` can re-emit the provenance — this is what
    /// lets the recursive `fib(k - 1)` call site (which always goes
    /// through `LetGet`) keep the self-recursion fast path intact.
    pub(crate) self_capture_let_slots: std::collections::HashMap<u32, (u32, u32)>,
    /// Phase F.W7 self-recursion fast path: captures_ptr LLVM param
    /// (param 1) of the enclosing lambda. Cached so the closure-call
    /// emitter can pass it straight into the recursive call without
    /// re-loading from the closure handle. `None` when emitting the
    /// entry / a helper (not a lambda body) — the self-recursion fast
    /// path is gated on this being `Some`.
    pub(crate) captures_ptr_param: Option<IntValue<'ctx>>,
    /// Phase D.2 fast-path entry: let-slot indices holding a
    /// virtualised closure stamped by an in-body `Op::MakeClosure`
    /// (carries `Provenance::FastPathClosure`). The `LetSet` that
    /// catches such a value stashes the `fn_table_idx` here so the
    /// matching `LetGet` can re-emit the provenance, keeping the
    /// `CallClosure` direct-call rewrite alive across the let chain.
    /// Empty when not emitting the fast-path entry.
    pub(crate) fast_path_closure_let_slots: std::collections::HashMap<u32, u32>,
    /// Phase L W3: let-slot indices holding a `Provenance::ConstString`
    /// value (i.e. the let was set from a value sourced — directly or
    /// via prior `LetGet` chains — from an `Op::ConstString`). The
    /// matching `LetGet` re-stamps the provenance so the downstream
    /// `Op::Add(String)` lowering can switch to the const-len /
    /// single-byte-store fast path. Each entry records (len, optional
    /// first_byte). Empty by default; entries survive only across
    /// inner-loop iterations because the W3 reduce shape's `s` let is
    /// re-set every iteration from the same const literal.
    pub(crate) const_string_let_slots: std::collections::HashMap<u32, (u32, Option<u8>)>,
    /// Devirtualisation (W18): let-slot indices holding a real
    /// arena-resident closure handle whose `fn_table_idx` is a
    /// compile-time constant (`Provenance::KnownClosure`). The `LetSet`
    /// that catches such a value stashes the `fn_table_idx` here so the
    /// matching `LetGet` re-stamps the provenance, letting the downstream
    /// `CallClosure` emit a direct call (LLVM inlines it) instead of the
    /// runtime `switch i32 %cc_fn_idx`. A non-known-closure `LetSet`
    /// against the same slot wipes the entry so a later `LetGet` cannot
    /// fraudulently claim a static target. Empty by default.
    pub(crate) known_closure_let_slots: std::collections::HashMap<u32, u32>,
    /// Devirtualisation (W18): `(capture_offset, captured_fn_table_idx)`
    /// pairs for the lambda body currently being emitted, identifying
    /// captures-struct offsets that hold a handle produced by a literal
    /// `MakeClosure` with a compile-time-constant `fn_table_idx` (a
    /// *known* closure that is NOT a self-capture). The capture-load
    /// prologue (`LocalGet(0); LoadI32AtAbsolute { offset }`) stamps
    /// [`Provenance::KnownClosure`] on the matching load so a body
    /// `CallClosure` against the capture emits a direct call. Seeded by
    /// [`build_known_capture_table`]; empty when emitting the entry /
    /// helpers or a lambda with no such captures.
    pub(crate) known_capture_offsets: Vec<(u32, u32)>,
}

/// Phase E.1: per-call inline-frame state. One entry per active
/// stdlib `Op::Call`; the callee body lowers against the topmost
/// frame.
pub(crate) struct InlineFrame<'ctx> {
    /// LLVM values bound to the callee's `LocalGet(0..arity)` reads.
    /// Order matches the IR's declared parameter order — the
    /// `Op::Call` site popped them from the caller's operand stack
    /// (top-of-stack = last param) and reversed.
    pub(crate) params: Vec<TypedValue<'ctx>>,
    /// Offset added to the callee's `LetGet/LetSet` indices so its
    /// let-bindings don't alias the caller's slots. Mirrors the
    /// cranelift backend's `let_offset`.
    pub(crate) let_offset: u32,
    /// Result alloca + exit basic block. The callee's `Op::Return`
    /// stores the popped value into the alloca and unconditionally
    /// branches to `exit_bb`; the caller continues from there with a
    /// matching load.
    pub(crate) ret_slot: PointerValue<'ctx>,
    /// LLVM type stored at [`Self::ret_slot`]. Pre-computed from the
    /// IR-declared `ret_ty` of the stdlib call so the caller-side
    /// load knows what width to read.
    pub(crate) ret_ty: IrType,
    /// Branch target for `Op::Return` inside the callee body. The
    /// caller positions the builder here after the inline finishes
    /// and pushes the loaded return value back onto the operand
    /// stack.
    pub(crate) exit_bb: inkwell::basic_block::BasicBlock<'ctx>,
}

/// Phase D.1 fast-path emission state. Carried inside [`Emit`] when
/// lowering the typed fast entry.
#[derive(Clone)]
pub(crate) struct FastEmit<'ctx> {
    pub(crate) profile: FastPathProfile,
    /// Alloca holding the i64 return value. Trailing `StoreField`
    /// at `profile.ret_offset` writes into this slot; `Op::Return`
    /// loads from it.
    pub(crate) ret_slot: PointerValue<'ctx>,
}

#[derive(Clone, Copy)]
pub(crate) struct TypedValue<'ctx> {
    pub(crate) val: IntValue<'ctx>,
    /// IR-level tag of `val`. Recorded so Phase C predicates that
    /// inspect operand types (signed-vs-unsigned cmp, F64 routing)
    /// have it on hand without re-deriving from LLVM bit width.
    /// Phase B never consumes this field; `#[allow(dead_code)]`
    /// keeps the lint clean while we're still wiring future Op
    /// support.
    #[allow(dead_code)]
    pub(crate) ty: IrType,
    /// Provenance hint used by [`Emit::emit_call_closure`] to detect
    /// self-recursive closure calls. Defaults to [`Provenance::None`]
    /// for every push that doesn't go through the lambda-prologue
    /// capture path; the closure-self-call fast path only fires when
    /// the consumed handle's provenance points at one of the lambda's
    /// own self-capture offsets.
    pub(crate) prov: Provenance,
}

/// Tracks where an [`IntValue`] on the operand stack came from so the
/// closure-call emitter can detect self-recursion without re-loading
/// the handle's captures pointer through arena indirection.
///
/// The W7 production source's `fib` closure captures itself, so every
/// recursive `fib(k - 1)` call site walks
/// `captures_ptr -> self_handle -> captures_ptr_field -> direct call`.
/// LLVM cannot fold the `captures_ptr_field` load back to the input
/// `captures_ptr` because the chain crosses `MakeClosure` in another
/// function (no IPA reach), so a pure post-O3 IR ends up with three
/// arena loads per recursion (`~10 ns/call ≈ +170 µs` over `fib(22)`).
///
/// The provenance bits below are enough to short-circuit:
///
/// * `OwnCapturesPtr` — the value is the lambda's own captures_ptr arg
///   (LLVM param 1). Produced by `Op::LocalGet(0)` inside a lambda.
/// * `OwnCaptureHandle { offset, self_fn_table_idx }` — the value is a
///   closure handle loaded from `captures_ptr + offset` and the
///   matching `MakeClosure` capture is self-recursive (handle points
///   back at the enclosing lambda whose `fn_table_idx ==
///   self_fn_table_idx`). Lets `Op::CallClosure` emit a direct call to
///   `closure_fn_table[self_fn_table_idx]` with the current
///   `captures_ptr` arg — no handle deref, no switch, no trap branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provenance {
    None,
    /// LLVM param 1 of the enclosing lambda — the captures_ptr arg.
    OwnCapturesPtr,
    /// Closure handle loaded from `captures_ptr + offset`; the matching
    /// MakeClosure capture is self-recursive, so the handle's
    /// `captures_ptr` field equals `OwnCapturesPtr` and the handle's
    /// `fn_table_idx` equals `self_fn_table_idx`.
    OwnCaptureHandle {
        #[allow(dead_code)]
        offset: u32,
        self_fn_table_idx: u32,
    },
    /// Phase D.2: closure handle materialised by a `MakeClosure` op
    /// inside the fast-path entry. The fast entry has no arena/state,
    /// so `MakeClosure` cannot bump-allocate the 8-byte handle record;
    /// instead the value is virtualised — we remember the
    /// `fn_table_idx` and rewrite the matching `CallClosure` into a
    /// direct call against the lambda function. The lambda's
    /// `(state, captures_ptr, args...)` signature is satisfied by
    /// passing null / zero for state / captures, which is sound for
    /// W7-style self-recursive closures whose post-O3 body drops
    /// both args.
    FastPathClosure {
        fn_table_idx: u32,
    },
    /// Devirtualisation (W18, 2026-05-30): the IntValue is a *real*
    /// arena-resident closure handle (`[fn_table_idx][captures_ptr]`)
    /// produced by a literal [`Op::MakeClosure`] whose `fn_table_idx` is
    /// a compile-time constant. Unlike [`Self::FastPathClosure`] the
    /// handle is fully materialised in the arena (the buffer-protocol
    /// entry has state + arena), so the matching `CallClosure` still
    /// loads the real `captures_ptr` from `handle + 4` — it only skips
    /// the runtime `switch i32 %cc_fn_idx` over `handle + 0`, because the
    /// handle's `fn_table_idx` word is *provably* this constant.
    ///
    /// Soundness: the value flows unmodified from the `MakeClosure` (or a
    /// `LetSet`/`LetGet` round-trip, or an inline-frame argument bind)
    /// to the `CallClosure`; there is exactly one possible callee, so the
    /// switch's runtime selection is statically decided. The slow-path
    /// `build_switch` stays for any handle that did *not* arrive with
    /// this provenance (a genuinely-dynamic dispatch). When the W18
    /// `_list_filter` predicate (a literal `(k) => is_prime(k, 2)`
    /// MakeClosure) is inlined into the bundled `list_int_filter` body,
    /// this lets the per-element predicate dispatch become a direct call
    /// LLVM then inlines, killing the hot-loop switch.
    KnownClosure {
        fn_table_idx: u32,
    },
    /// Phase L W3 (2026-05-28): the IntValue is an i32 arena offset to a
    /// `[len:u32 LE][payload]` String record whose payload was placed in
    /// the const-pool prefix at module build time, so its length is
    /// known at compile time. Carried by `Op::ConstString` and
    /// propagated through `Op::LetSet { ty: String }` →
    /// `Op::LetGet { ty: String }` so `Op::Add(String)` can feed the
    /// const length to LLVM (memcpy intrinsic with const size lowers
    /// to inline stores) and skip the per-iter `[len]` header reload.
    ///
    /// Single-byte payloads (the W3 reduce hot loop's `"a"`) further
    /// expose `first_byte` so the in-place fast path can emit a single
    /// `i8 store` instead of `memcpy` — bypassing the LLVM lowering
    /// pass altogether for the dominant reduce shape.
    ConstString {
        len: u32,
        /// `Some(byte)` when `len == 1` so the lowering can emit an
        /// inline `store i8 byte, dst` instead of a memcpy intrinsic.
        /// `None` for longer payloads (LLVM's memcpy intrinsic
        /// lowering still handles those well once the size is const).
        first_byte: Option<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LabelKind {
    /// `Br` jumps **past** the block (forward exit).
    Block,
    /// `Br` jumps **back** to the loop header (continue).
    Loop,
}

#[derive(Clone)]
pub(crate) struct LabelFrame<'ctx> {
    /// Header basic block. For Block this is unused for branching
    /// (we never branch backward to the start of a block); for Loop
    /// it's the target of a `Br` (continue).
    pub(crate) header_bb: inkwell::basic_block::BasicBlock<'ctx>,
    /// Tail basic block — what code after the block / after the
    /// loop falls through to. For Block this is the `Br` target;
    /// for Loop the surrounding code lives here.
    pub(crate) tail_bb: inkwell::basic_block::BasicBlock<'ctx>,
    pub(crate) kind: LabelKind,
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ctx: &'ctx Context,
        builder: &'b Builder<'ctx>,
        module: &'b LlvmModule<'ctx>,
        func: FunctionValue<'ctx>,
        shape: EntryShape,
        arena_base_ptr: Option<PointerValue<'ctx>>,
        state_ptr: Option<PointerValue<'ctx>>,
        buffer_return_size: u32,
        const_pool: &'cp ConstPool,
    ) -> Self {
        Self {
            ctx,
            builder,
            func,
            module,
            shape,
            arena_base_ptr,
            state_ptr,
            stack: Vec::with_capacity(8),
            let_slots: std::collections::HashMap::new(),
            param_base: 0,
            label_stack: Vec::new(),
            name_seq: 0,
            buffer_return_size,
            fast_path: None,
            helper_table: None,
            helper_ret_ty: None,
            llvm_trap_fn: None,
            const_pool,
            inline_frames: Vec::new(),
            needs_tail_cursor: false,
            last_const_string: None,
            closure_fn_table: Vec::new(),
            record_locals: std::collections::HashMap::new(),
            self_capture_offsets: Vec::new(),
            self_capture_let_slots: std::collections::HashMap::new(),
            captures_ptr_param: None,
            fast_path_closure_let_slots: std::collections::HashMap::new(),
            const_string_let_slots: std::collections::HashMap::new(),
            known_closure_let_slots: std::collections::HashMap::new(),
            known_capture_offsets: Vec::new(),
        }
    }

    pub(crate) fn next_name(&mut self, hint: &str) -> String {
        self.name_seq += 1;
        format!("{hint}_{}", self.name_seq)
    }

    // -- stack helpers --------------------------------------------------

    pub(crate) fn push(&mut self, v: IntValue<'ctx>, ty: IrType) {
        self.stack.push(TypedValue {
            val: v,
            ty,
            prov: Provenance::None,
        });
    }

    /// Push a value while attaching a [`Provenance`] tag. Currently
    /// only emitted by the lambda-prologue capture path
    /// (`LocalGet(0)` → `LoadI32AtAbsolute` → `LetSet/LetGet`) so
    /// `emit_call_closure` can short-circuit self-recursive calls.
    pub(crate) fn push_with_prov(&mut self, v: IntValue<'ctx>, ty: IrType, prov: Provenance) {
        self.stack.push(TypedValue { val: v, ty, prov });
    }

    /// Phase F.W7 self-recursion fast path: peek the operand stack's
    /// top-of-stack provenance without consuming it and return the
    /// matching [`Provenance::OwnCaptureHandle`] when the top is the
    /// lambda's captures_ptr and `offset` matches a recorded self-
    /// recursive capture offset. Returns `None` otherwise — the
    /// caller then leaves the produced value's provenance at
    /// [`Provenance::None`] and the closure-call emitter falls back
    /// to the slow-path switch dispatch.
    ///
    /// Caller uses this **after** `emit_load_at_absolute` pops the
    /// base; we read the stack top here before that pop runs, so
    /// the lookup remains correct (the base is still on top when
    /// the dispatcher arm fires).
    pub(crate) fn peek_self_capture_provenance(&self, offset: u32) -> Option<Provenance> {
        let top = self.stack.last()?;
        if !matches!(top.prov, Provenance::OwnCapturesPtr) {
            return None;
        }
        // Self-recursive capture wins (its `captures_ptr`-reuse direct
        // path is strictly cheaper than re-loading the handle's
        // captures_ptr field).
        for (cap_offset, self_fn_table_idx) in &self.self_capture_offsets {
            if *cap_offset == offset {
                return Some(Provenance::OwnCaptureHandle {
                    offset,
                    self_fn_table_idx: *self_fn_table_idx,
                });
            }
        }
        // Devirtualisation (W18): a capture of a known (non-self)
        // closure. Stamp `KnownClosure` so the body's `CallClosure`
        // against the capture emits a direct call (still loading the
        // capture's own captures_ptr) instead of the runtime switch.
        for (cap_offset, captured_fn_table_idx) in &self.known_capture_offsets {
            if *cap_offset == offset {
                return Some(Provenance::KnownClosure {
                    fn_table_idx: *captured_fn_table_idx,
                });
            }
        }
        None
    }

    pub(crate) fn pop(&mut self, ip_hint: &str) -> Result<TypedValue<'ctx>, LlvmError> {
        self.stack.pop().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "operand stack underflow at {ip_hint}: producer emitted an Op with no matching push"
            ))
        })
    }

    pub(crate) fn pop_int(&mut self, ip_hint: &str) -> Result<IntValue<'ctx>, LlvmError> {
        self.pop(ip_hint).map(|tv| tv.val)
    }

    // -- locals / lets --------------------------------------------------

    pub(crate) fn lookup_param(&self, idx: u32) -> Result<IntValue<'ctx>, LlvmError> {
        let llvm_idx = self
            .param_base
            .checked_add(idx)
            .ok_or_else(|| LlvmError::Codegen(format!("LocalGet({idx}): param idx overflow")))?;
        let p = self.func.get_nth_param(llvm_idx).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "LocalGet({idx}) -> llvm param #{llvm_idx} out of range; function has {} param(s)",
                self.func.count_params()
            ))
        })?;
        match p {
            BasicValueEnum::IntValue(v) => Ok(v),
            other => Err(LlvmError::Codegen(format!(
                "LocalGet({idx}) llvm param #{llvm_idx} is {other:?}, expected IntValue"
            ))),
        }
    }

    pub(crate) fn ensure_let_slot(&mut self, idx: u32, ty: IrType) -> Result<PointerValue<'ctx>, LlvmError> {
        if let Some((ptr, existing_ty)) = self.let_slots.get(&idx) {
            if *existing_ty != ty {
                return Err(LlvmError::Codegen(format!(
                    "let-slot {idx} aliased: previous type {existing_ty:?}, new type {ty:?}"
                )));
            }
            return Ok(*ptr);
        }
        // Allocate in the function's entry block so the alloca is
        // hoisted out of any loop body. inkwell's `build_alloca`
        // emits at the current position, so we temporarily reposition.
        let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
            LlvmError::Codegen("ensure_let_slot: function has no entry block".into())
        })?;
        let cur = self.builder.get_insert_block();
        // Position at the start of the entry block so allocas group
        // at the top — LLVM mem2reg requires this canonical layout
        // to promote slots into SSA.
        if let Some(first_instr) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first_instr);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match ty {
            // AOT-1: F64 rides as i64 bits on the virtual stack, so its
            // let-slot is the same 64-bit-wide integer alloca as I64.
            // The `(idx, ty)` aliasing key keeps an I64 and an F64 slot
            // for the same index distinct, so the bit pattern never gets
            // reinterpreted across types.
            IrType::I64 | IrType::F64 => self.ctx.i64_type().into(),
            // Phase E.1: String / List* arena offsets ride on an i32
            // slot — matches the cranelift backend's pointer-as-i32
            // wire representation.
            //
            // Phase F.W7: `Closure` joins the i32-wide variants
            // (closure handle is an arena-relative i32 pointer at
            // the IR / cranelift / LLVM boundary alike).
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => self.ctx.i32_type().into(),
        };
        let name = format!("let_{idx}");
        let ptr = self
            .builder
            .build_alloca(llvm_ty, &name)
            .map_err(|e| LlvmError::Codegen(format!("let-slot {idx} alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }
        self.let_slots.insert(idx, (ptr, ty));
        Ok(ptr)
    }

    // -- entry point ----------------------------------------------------

    pub(crate) fn lower_body(&mut self, body: &[TaggedOp]) -> Result<(), LlvmError> {
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        // After `Op::Return` we positioned at a fresh "after_return_cont"
        // block which is dead and unterminated. Seal it with
        // `unreachable` so LLVM's verifier accepts the module. Same
        // pattern applies to the post-`Br` continuation block.
        if let Some(cur) = self.builder.get_insert_block() {
            if cur.get_terminator().is_none() {
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("trailing unreachable: {e}")))?;
            }
        }
        Ok(())
    }

    // -- per-op lowering ------------------------------------------------

    pub(crate) fn lower_op(&mut self, ip: usize, tagged: &TaggedOp) -> Result<(), LlvmError> {
        let ip_hint = format!("ip={ip} op={:?}", tagged.op);
        // Phase H const-needle fast path: capture (and clear) the
        // `Op::ConstString` peek-state at the very start of every
        // `lower_op` dispatch. The `Op::Call` arm consults `prev_const_string`
        // to decide between the inline byte-scan and the extern shim.
        // Every other arm leaves `self.last_const_string` at `None` —
        // the only re-populator is the `Op::ConstString` arm at its
        // tail. Result: `prev_const_string.is_some()` iff the prior
        // emitted op was `Op::ConstString` and its value is still the
        // top-of-stack (no intervening op consumed it).
        let prev_const_string = self.last_const_string.take();
        match &tagged.op {
            // ---- literals ----
            Op::ConstI64(v) => {
                let c = self.ctx.i64_type().const_int(*v as u64, true);
                self.push(c, IrType::I64);
            }
            Op::ConstI32(v) => {
                let c = self.ctx.i32_type().const_int(*v as u32 as u64, false);
                self.push(c, IrType::I32);
            }
            Op::ConstBool(b) => {
                // Bool occupies an i32 slot on the IR's virtual stack.
                let c = self.ctx.i32_type().const_int(u64::from(*b), false);
                self.push(c, IrType::Bool);
            }
            Op::ConstF64(v) => {
                // AOT-1: materialise the `double` literal then bit-cast
                // to i64 so the operand stack stays integer-typed
                // (Option B). `v` is an `OrderedFloat<f64>`.
                let f = self.ctx.f64_type().const_float(v.into_inner());
                let bits = self
                    .builder
                    .build_bit_cast(f, self.ctx.i64_type(), &self.next_name("constf64_bits"))
                    .map_err(|e| LlvmError::Codegen(format!("ConstF64 bitcast: {e}")))?
                    .into_int_value();
                self.push(bits, IrType::F64);
            }

            // ---- locals / lets ----
            Op::LocalGet(idx) => {
                // Phase E.1: an active inline frame redirects
                // `LocalGet(i)` to the inlined call's `i`-th argument
                // instead of the entry-function's LLVM params.
                if let Some(frame) = self.inline_frames.last() {
                    let i = *idx as usize;
                    let tv = frame.params.get(i).ok_or_else(|| {
                        LlvmError::Codegen(format!(
                            "inline LocalGet({idx}) out of range — callee has {} params",
                            frame.params.len()
                        ))
                    })?;
                    // Preserve provenance across the inline-frame argument
                    // bind. The bundled `list_int_filter` body reads its
                    // closure parameter via `LocalGet(1)`; when the caller
                    // passed a literal `MakeClosure` (a `KnownClosure`
                    // handle), forwarding that provenance lets the body's
                    // per-element `CallClosure` devirtualise into a direct
                    // call. Only `KnownClosure` is propagated here — the
                    // self-recursion / fast-path-entry tags depend on the
                    // current function's `captures_ptr_param` / fast-path
                    // state, which a *callee* inline frame does not share,
                    // so forwarding those would be unsound.
                    let (val, prov) = (tv.val, tv.prov);
                    match prov {
                        Provenance::KnownClosure { .. } => {
                            self.push_with_prov(val, tv.ty, prov);
                        }
                        _ => self.push(val, tv.ty),
                    }
                } else {
                    let p = self.lookup_param(*idx)?;
                    // The legacy envelope walks all-i64; the buffer envelope
                    // walks (i32 ×4, i64). The IR has the right type on
                    // the param descriptor, but we don't carry it through
                    // LocalGet — re-derive from the LLVM param width.
                    let width = p.get_type().get_bit_width();
                    let ty = if width == 32 {
                        IrType::I32
                    } else {
                        IrType::I64
                    };
                    // Phase F.W7 self-recursion fast path: tag
                    // `LocalGet(0)` inside a lambda body with
                    // [`Provenance::OwnCapturesPtr`] so the prologue
                    // capture-load chain can stamp
                    // [`Provenance::OwnCaptureHandle`] on self-
                    // recursive handles. Only fires inside a lambda
                    // (param_base == 1 means the LLVM param 0 is
                    // `*state` and param 1 is the captures_ptr arg);
                    // the entry / helpers leave provenance at
                    // `None`.
                    if *idx == 0 && self.captures_ptr_param.is_some() {
                        self.push_with_prov(p, ty, Provenance::OwnCapturesPtr);
                    } else {
                        self.push(p, ty);
                    }
                }
            }
            Op::LetSet { idx, ty } => {
                let v = self.pop(&ip_hint)?;
                let mapped = self.remap_let_idx(*idx);
                let slot = self.ensure_let_slot(mapped, *ty)?;
                // Coerce on bool / null where the producer pushed an i32
                // slot but the let-slot was declared as the canonical
                // 32-bit width.
                let stored = self.coerce_to_let_ty(v, *ty)?;
                self.builder
                    .build_store(slot, stored)
                    .map_err(|e| LlvmError::Codegen(format!("LetSet store: {e}")))?;
                // Phase F.W7 self-recursion fast path: when storing a
                // closure handle whose provenance points back at the
                // enclosing lambda, remember the let-slot so a later
                // `LetGet` resurrects the same provenance. This is
                // what bridges the prologue's capture-load chain
                // (`LocalGet(0); LoadI32AtAbsolute { offset }; LetSet
                // { idx, Closure }`) and the recursive call site
                // (`LetGet { idx, Closure }; ...; CallClosure`).
                if let Provenance::OwnCaptureHandle {
                    offset,
                    self_fn_table_idx,
                } = v.prov
                {
                    if matches!(*ty, IrType::Closure) {
                        self.self_capture_let_slots
                            .insert(mapped, (offset, self_fn_table_idx));
                    }
                }
                // Phase D.2 fast-path entry: when storing a virtualised
                // closure produced by an in-body `MakeClosure` (no
                // arena/state available), remember the `fn_table_idx`
                // so the matching `LetGet` re-emits the provenance and
                // the downstream `CallClosure` can rewrite into a
                // direct call.
                if let Provenance::FastPathClosure { fn_table_idx } = v.prov {
                    if matches!(*ty, IrType::Closure) {
                        self.fast_path_closure_let_slots
                            .insert(mapped, fn_table_idx);
                    }
                }
                // Devirtualisation (W18): propagate `KnownClosure`
                // across the `LetSet` → `LetGet` chain so a closure
                // handle stored into a let then read back at a
                // `CallClosure` site keeps its compile-time
                // `fn_table_idx`. A `LetSet { Closure }` of any *other*
                // provenance overwrites the slot with a value we cannot
                // prove is the same single closure, so drop the entry —
                // a later `LetGet` then falls back to the runtime
                // switch. This invalidation is what keeps a slot that is
                // reassigned to a dynamically-chosen closure correct.
                match (v.prov, *ty) {
                    (Provenance::KnownClosure { fn_table_idx }, IrType::Closure) => {
                        self.known_closure_let_slots.insert(mapped, fn_table_idx);
                    }
                    (_, IrType::Closure) => {
                        self.known_closure_let_slots.remove(&mapped);
                    }
                    _ => {}
                }
                // Phase L W3: propagate `Provenance::ConstString`
                // across the `LetSet` → `LetGet` chain so the reduce
                // closure's `s` (set every iteration from the same
                // const literal "a" in the W3 source) can be picked
                // up by `Op::Add(String)` as a const-len operand.
                // Any non-const-string `LetSet` against the same idx
                // wipes the entry below.
                match (v.prov, *ty) {
                    (Provenance::ConstString { len, first_byte }, IrType::String) => {
                        self.const_string_let_slots
                            .insert(mapped, (len, first_byte));
                    }
                    (_, IrType::String) => {
                        // A non-const value just overwrote the slot —
                        // drop any stale const-string record so a
                        // later `LetGet` cannot fraudulently claim
                        // const-len status.
                        self.const_string_let_slots.remove(&mapped);
                    }
                    _ => {}
                }
            }
            Op::LetGet { idx, ty } => {
                // Phase E.1: remap the callee's let-idx against the
                // active inline frame so concurrent stdlib inlines
                // don't clash on slot numbers.
                let mapped = self.remap_let_idx(*idx);
                let slot = self.ensure_let_slot(mapped, *ty)?;
                let llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match *ty {
                    // AOT-1: F64 rides as i64 bits, so its let-slot loads
                    // back as an i64 (the raw bit pattern, reinterpreted
                    // as `double` only at the arithmetic / store site).
                    IrType::I64 | IrType::F64 => self.ctx.i64_type().into(),
                    IrType::I32
                    | IrType::Bool
                    | IrType::Null
                    | IrType::String
                    | IrType::ListInt
                    | IrType::ListFloat
                    | IrType::ListBool
                    | IrType::ListString
                    | IrType::ListSchema
                    | IrType::Closure => self.ctx.i32_type().into(),
                };
                let name = self.next_name("letget");
                let v = self
                    .builder
                    .build_load(llvm_ty, slot, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LetGet load: {e}")))?
                    .into_int_value();
                // Phase F.W7 self-recursion fast path: when the let-slot
                // was populated by the lambda prologue's self-capture
                // load chain, re-stamp the matching
                // [`Provenance::OwnCaptureHandle`] so the recursive
                // call site (which reads the closure handle via
                // `LetGet`) keeps the fast-path tag alive.
                if matches!(*ty, IrType::Closure) {
                    if let Some(&(offset, self_fn_table_idx)) =
                        self.self_capture_let_slots.get(&mapped)
                    {
                        self.push_with_prov(
                            v,
                            *ty,
                            Provenance::OwnCaptureHandle {
                                offset,
                                self_fn_table_idx,
                            },
                        );
                    } else if let Some(&fn_table_idx) =
                        self.fast_path_closure_let_slots.get(&mapped)
                    {
                        // Phase D.2 fast-path entry: re-stamp the
                        // virtualised-closure tag so the matching
                        // `CallClosure` keeps the direct-call rewrite
                        // available.
                        self.push_with_prov(v, *ty, Provenance::FastPathClosure { fn_table_idx });
                    } else if let Some(&fn_table_idx) = self.known_closure_let_slots.get(&mapped) {
                        // Devirtualisation (W18): re-stamp `KnownClosure`
                        // so a `CallClosure` reading this handle through
                        // the let chain emits a direct call (still
                        // loading the real captures_ptr) instead of the
                        // runtime switch.
                        self.push_with_prov(v, *ty, Provenance::KnownClosure { fn_table_idx });
                    } else {
                        self.push(v, *ty);
                    }
                } else if matches!(*ty, IrType::String) {
                    // Phase L W3: re-stamp `Provenance::ConstString`
                    // when the let-slot is known to hold a value
                    // sourced from `Op::ConstString`. Crucial for the
                    // reduce closure's `s` operand — the iter-body
                    // sets `s` from a const literal then `LetGet`s it
                    // into the `Op::Add(String)` rhs, so without
                    // propagation the const-len fast path can never
                    // fire across the let chain.
                    if let Some(&(len, first_byte)) = self.const_string_let_slots.get(&mapped) {
                        self.push_with_prov(v, *ty, Provenance::ConstString { len, first_byte });
                    } else {
                        self.push(v, *ty);
                    }
                } else {
                    self.push(v, *ty);
                }
            }

            // ---- arithmetic ----
            Op::Add(ty) => match ty {
                // Phase E.1: `Op::Add(IrType::String)` is the
                // pair-wise String + String form (the StrConcatN
                // fold only fires for compile-time-known chains —
                // `reduce("", (acc, s) => acc + s)` lowers to a
                // per-iter `Add(String)`).
                //
                // Phase I (W3 string-concat gap close): emit the
                // in-place-append fast path. The W3 reduce hot loop
                // walks `acc = acc + "a"` for N iters; under the
                // historical inlined-`concat` body that turned into
                // an O(N²) byte-copy storm because every iter
                // reallocated a fresh scratch record. The new
                // helper recognises the "lhs is the most recent
                // scratch alloc" case at runtime and extends the
                // record in place — total work drops to O(N) bytes,
                // matching `String::push_str`. The slow path stays
                // bit-identical with the historical lowering so
                // mixed-source string adds (const-pool literals,
                // out-of-order scratch records) still produce a
                // fresh record.
                IrType::String => self.emit_str_add_inplace_or_concat(&ip_hint)?,
                _ => self.emit_binop(&ip_hint, *ty, BinOp::Add)?,
            },
            Op::Sub(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Sub)?,
            Op::Mul(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Mul)?,
            Op::Div(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Div)?,
            Op::Mod(ty) => self.emit_binop(&ip_hint, *ty, BinOp::Mod)?,
            Op::BitAnd(ty) => self.emit_binop(&ip_hint, *ty, BinOp::BitAnd)?,
            Op::ConvertI64ToF64 => self.emit_convert_i64_to_f64(&ip_hint)?,

            // ---- comparisons ----
            Op::Eq(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::EQ)?,
            Op::Ne(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::NE)?,
            Op::Lt(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SLT)?,
            Op::Le(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SLE)?,
            Op::Gt(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SGT)?,
            Op::Ge(ty) => self.emit_cmp(&ip_hint, *ty, IntPredicate::SGE)?,

            // ---- buffer-protocol I/O ----
            Op::LoadField { offset, ty } => self.emit_load_field(*offset, *ty)?,
            Op::StoreField { offset, ty } => self.emit_store_field(&ip_hint, *offset, *ty)?,

            // ---- pointer-indirect param loads (Phase 2 relon-rs surface) ----
            // String / List* `#main` parameters arrive in the input
            // buffer as a 4-byte buffer-relative offset to a tail
            // record. The IR's lowering pass emits `Op::LoadStringPtr`
            // (and its List* siblings) instead of `Op::LoadField {
            // ty: String }` so the dispatch stays unambiguous; we
            // share the same `emit_load_pointer_indirect_param` impl
            // for all variants.
            Op::LoadStringPtr { offset } => {
                self.emit_load_pointer_indirect_param(*offset, IrType::String)?
            }
            Op::LoadListIntPtr { offset } => {
                self.emit_load_pointer_indirect_param(*offset, IrType::ListInt)?
            }
            Op::LoadListFloatPtr { offset } => {
                self.emit_load_pointer_indirect_param(*offset, IrType::ListFloat)?
            }
            Op::LoadListBoolPtr { offset } => {
                self.emit_load_pointer_indirect_param(*offset, IrType::ListBool)?
            }
            Op::LoadListStringPtr { offset } => {
                self.emit_load_pointer_indirect_param(*offset, IrType::ListString)?
            }
            Op::LoadListSchemaPtr { offset } => {
                self.emit_load_pointer_indirect_param(*offset, IrType::ListSchema)?
            }

            // ---- ReadStringLen (Phase 2 — backs `length(s)` / `len(xs)`) ----
            // Pop arena-relative i32 record pointer, load the leading
            // 4-byte length prefix, zext to i64 and push. Used by the
            // bundled stdlib `length` (String) / `list_*_length` bodies
            // — every list record shares the `[len: u32 LE]` prefix
            // with String, so a single lowering covers both.
            Op::ReadStringLen => self.emit_read_string_len(&ip_hint)?,

            // ---- control flow ----
            Op::Block { result_ty, body } => self.emit_block(*result_ty, body)?,
            Op::Loop { result_ty, body } => self.emit_loop(*result_ty, body)?,
            Op::Br { label_depth } => self.emit_br(*label_depth)?,
            Op::BrIf { label_depth } => self.emit_br_if(&ip_hint, *label_depth)?,
            Op::If {
                result_ty,
                then_body,
                else_body,
            } => self.emit_if(&ip_hint, *result_ty, then_body, else_body)?,

            // ---- return ----
            Op::Return => self.emit_return(&ip_hint)?,

            // ---- Phase E.1: const-data pool ----
            Op::ConstString { idx, value } => {
                let off = self
                    .const_pool
                    .string_offsets
                    .get(idx)
                    .copied()
                    .ok_or_else(|| {
                        LlvmError::Codegen(format!(
                            "Op::ConstString {{ idx: {idx} }}: missing const-pool entry — \
                         did the host forget to lay out the pool blob before dispatch?"
                        ))
                    })?;
                let c = self.ctx.i32_type().const_int(u64::from(off), false);
                // Phase L W3: stamp const-len provenance so the
                // downstream `Op::Add(String)` lowering (via
                // `emit_str_add_inplace_or_concat`) can use the
                // compile-time-known length to elide the per-iter
                // `[len]` header reload and replace the rhs memcpy
                // with a single byte store when the literal is one
                // byte (the dominant cmp_lua W3 reduce shape). The
                // provenance only survives across `LetSet`/`LetGet`
                // for `IrType::String` (tracked in
                // `const_string_let_slots`) so non-String consumers
                // never observe it.
                let bytes = value.as_bytes();
                let len_u32 = u32::try_from(bytes.len()).map_err(|_| {
                    LlvmError::Codegen("ConstString length exceeds u32 range".into())
                })?;
                let first_byte = if bytes.len() == 1 {
                    Some(bytes[0])
                } else {
                    None
                };
                self.push_with_prov(
                    c,
                    IrType::String,
                    Provenance::ConstString {
                        len: len_u32,
                        first_byte,
                    },
                );
                // Phase H peek-state: record the literal bytes so the
                // next `lower_op` call can detect `Op::Call(contains)`
                // with this string still at top-of-stack and switch
                // to the inline byte-scan instead of the extern shim.
                // Cleared at the start of every `lower_op` — see the
                // `prev_const_string.take()` line at the dispatch
                // head — so a single intervening op (Push / Pop /
                // Add / ...) drops the optimisation cleanly.
                self.last_const_string = Some(bytes.to_vec());
            }

            // ---- Phase E.1: raw-memory primitives ----
            Op::LoadI32AtAbsolute { offset } => {
                // Phase F.W7 self-recursion fast path: when the base
                // (top-of-stack at this point) is the lambda's own
                // captures_ptr arg and the offset matches a recorded
                // self-recursive capture slot, the result is a
                // closure handle whose backing struct points back at
                // the enclosing lambda. Stash the provenance hint
                // so the downstream `LetSet/LetGet/CallClosure` chain
                // can short-circuit the indirect dispatch. The
                // sniff peeks at the stack-top without mutating it;
                // the actual load still flows through
                // `emit_load_at_absolute` so we don't fork the
                // raw-memory primitive's lowering.
                let prov_hint = self.peek_self_capture_provenance(*offset);
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::I32)?;
                if let Some(prov) = prov_hint {
                    if let Some(top) = self.stack.last_mut() {
                        top.prov = prov;
                    }
                }
            }
            Op::LoadI64AtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::I64)?
            }
            Op::LoadI8UAtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::I8U)?
            }
            Op::LoadF64AtAbsolute { offset } => {
                self.emit_load_at_absolute(&ip_hint, *offset, AbsLoad::F64)?
            }
            Op::StoreI32AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::I32)?
            }
            Op::StoreI64AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::I64)?
            }
            Op::StoreI8AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::I8)?
            }
            Op::StoreF64AtAbsolute { offset } => {
                self.emit_store_at_absolute(&ip_hint, *offset, AbsStore::F64)?
            }
            Op::MemcpyAtAbsolute => self.emit_memcpy_at_absolute(&ip_hint)?,
            Op::AllocScratch { size_bytes } => self.emit_alloc_scratch_static(*size_bytes)?,
            Op::AllocScratchDyn => self.emit_alloc_scratch_dyn(&ip_hint)?,
            Op::StrConcatN { operand_count } => self.emit_str_concat_n(&ip_hint, *operand_count)?,

            // ---- Phase E.1 + E.2 call dispatch ----
            // stdlib indices (#278) route through the bundled-body
            // inline path (`emit_call_stdlib`); user-defined indices
            // (#279) resolve through the helper table populated by
            // `emit_module_funcs`.
            Op::Call {
                fn_index,
                arg_count,
                param_tys,
                ret_ty,
            } => {
                let stdlib_count = relon_ir::stdlib::stdlib_function_count();
                // Phase F.1: `contains(haystack, needle) -> Bool` short-
                // circuit. The bundled stdlib body is a hand-transcribed
                // O(s_len * p_len) byte scan that defeats LLVM's auto-
                // vectoriser on the inner compare loop (every iter
                // reloads the needle bytes through a let-slot). On the
                // W4 / W4_long cmp_lua rows that turns into a 3.4× /
                // 256× gap vs LuaJIT (which uses SIMD-accelerated
                // `string.find`). Route the call through the host shim
                // `relon_llvm_str_contains_arena` which defers to
                // `core::str::contains` — std's substring search backs
                // single-byte needles with SIMD `memchr` and uses a
                // Two-Way matcher for longer needles, closing the gap
                // without inventing a Relon-specific SIMD path.
                if *fn_index < stdlib_count
                    && relon_ir::stdlib::stdlib_function_index("contains") == Some(*fn_index)
                    && *arg_count == 2
                    && param_tys == &[IrType::String, IrType::String]
                    && *ret_ty == IrType::Bool
                {
                    // Phase H: when the needle was pushed by the
                    // immediately-preceding `Op::ConstString` (peek
                    // state populated at `lower_op` head), inline a
                    // tight byte-scan against the literal bytes.
                    // Skips the `relon_llvm_str_contains_arena` FFI
                    // boundary entirely — ~10-15 cycles of prologue /
                    // epilogue / IC atomic loads per call. The W4 /
                    // W4_long hot loops always hit this path (needle
                    // = `"x"` literal); dynamic-needle callers (e.g.
                    // `filter((s) => s.contains(other))` where
                    // `other` flows in via an outer let-slot) fall
                    // through to the existing Phase G extern shim.
                    if let Some(needle_bytes) = prev_const_string.as_deref() {
                        self.emit_str_contains_const_needle(&ip_hint, needle_bytes)?;
                    } else {
                        self.emit_str_contains_extern(&ip_hint)?;
                    }
                } else if *fn_index < stdlib_count {
                    self.emit_call_stdlib(&ip_hint, *fn_index, *arg_count, param_tys, *ret_ty)?
                } else {
                    self.emit_call(&ip_hint, *fn_index, *arg_count, param_tys, *ret_ty)?
                }
            }

            // ---- Phase F.W7: anon-Dict-return record ops ----
            // The IR lowering pass uses `AllocRootRecord` to bind a
            // per-record-local i32 alloca to `0` (the root sits at
            // `out_ptr + 0`); subsequent `StoreFieldAtRecord` ops use
            // the alloca-resident offset to compute the destination
            // address in the output buffer's fixed area.
            Op::AllocRootRecord { record_local_idx } => {
                self.emit_alloc_root_record(*record_local_idx)?
            }
            Op::StoreFieldAtRecord {
                record_local_idx,
                offset,
                ty,
            } => self.emit_store_field_at_record(&ip_hint, *record_local_idx, *offset, *ty)?,

            // ---- Phase F.W7: closure-as-value primitives ----
            Op::MakeClosure {
                fn_table_idx,
                captures,
                captures_size,
            } => self.emit_make_closure(&ip_hint, *fn_table_idx, captures, *captures_size)?,
            Op::CallClosure { param_tys, ret_ty } => {
                self.emit_call_closure(&ip_hint, param_tys, *ret_ty)?
            }

            // ---- not yet lowered by the LLVM AOT backend ----
            // These variants are intentionally unsupported in the
            // current envelope (Phase E.1 covers scalar arith + control
            // flow + the raw-memory / closure / buffer primitives the
            // bundled bodies need). They are listed EXPLICITLY rather
            // than swept up by a `_ =>` wildcard so that adding a new
            // `Op` variant fails to compile here — forcing a deliberate
            // decision (lower it, or add it to this unsupported set)
            // instead of silently surfacing as a runtime codegen error.
            // The behaviour is identical to the previous catch-all: a
            // `LlvmError::Codegen` that lets the host fall back.
            Op::ConstListInt { .. }
            | Op::ConstListFloat { .. }
            | Op::ConstListBool { .. }
            | Op::ConstListString { .. }
            | Op::DictGetByStringKey { .. }
            | Op::ListGetByIntIdx { .. }
            | Op::AllocSubRecord { .. }
            | Op::PushRecordBase { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. }
            | Op::Select { .. }
            | Op::LoadFieldAtAbsolute { .. }
            | Op::LoadSchemaPtr { .. }
            | Op::CallNative { .. }
            | Op::CheckCap { .. }
            | Op::BrTable { .. }
            | Op::Trap { .. }
            | Op::CaseFoldTableAddr { .. }
            | Op::CombiningMarkRangesAddr
            | Op::WhitespaceRangesAddr
            | Op::DecompTableAddr { .. }
            | Op::CccTableAddr
            | Op::CompositionTableAddr
            | Op::FullCaseFoldTableAddr { .. }
            | Op::CasedRangesAddr
            | Op::CaseIgnorableRangesAddr
            | Op::TurkishCaseFoldTableAddr { .. } => {
                return Err(LlvmError::Codegen(format!(
                    "unsupported op (Phase E.1 envelope): {:?} at ip={ip}",
                    tagged.op
                )));
            }
        }
        Ok(())
    }

    // -- Phase E.1: inline-call frame helpers --------------------------

    /// Translate a callee `LetGet/LetSet` index against the topmost
    /// inline frame. Mirrors cranelift's `remap_let_idx`.
    pub(crate) fn remap_let_idx(&self, idx: u32) -> u32 {
        match self.inline_frames.last() {
            Some(frame) => frame.let_offset.saturating_add(idx),
            None => idx,
        }
    }



    // -- helpers --------------------------------------------------------

    pub(crate) fn coerce_to_let_ty(
        &self,
        tv: TypedValue<'ctx>,
        target: IrType,
    ) -> Result<BasicValueEnum<'ctx>, LlvmError> {
        let want_width = match target {
            // AOT-1: F64 rides as i64 bits, so its let-slot is 64-wide
            // (same as I64). Coercion stays a width match — never an
            // int<->float cast — because the stack value is the raw
            // bit pattern, not a `double`.
            IrType::I64 | IrType::F64 => 64,
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => 32,
        };
        let have_width = tv.val.get_type().get_bit_width();
        if have_width == want_width {
            return Ok(tv.val.into());
        }
        let target_ty = if want_width == 64 {
            self.ctx.i64_type()
        } else {
            self.ctx.i32_type()
        };
        if have_width < want_width {
            self.builder
                .build_int_z_extend(tv.val, target_ty, "let_zext")
                .map(|v| v.as_basic_value_enum())
                .map_err(|e| LlvmError::Codegen(format!("let zext: {e}")))
        } else {
            self.builder
                .build_int_truncate(tv.val, target_ty, "let_trunc")
                .map(|v| v.as_basic_value_enum())
                .map_err(|e| LlvmError::Codegen(format!("let trunc: {e}")))
        }
    }









    // -- control flow ---------------------------------------------------






}



/// Inline lookup table used by `emit_load_field`. Picks the LLVM
/// integer type + the IR tag we push back onto the operand stack
/// for a Phase-B-supported scalar field type.
impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {


}

// ---------------------------------------------------------------------------
// Phase E.1: raw-memory primitives, scratch allocator, StrConcatN.
// ---------------------------------------------------------------------------



impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {














    /// Map an `IrType` to the LLVM int type used for the operand stack
    /// representation. Used by `Op::MakeClosure` capture reads and
    /// `Op::CallClosure` return loads.
    pub(crate) fn ir_ty_to_llvm_int(&self, ty: IrType) -> Result<inkwell::types::IntType<'ctx>, LlvmError> {
        match ty {
            IrType::I64 | IrType::F64 => Ok(self.ctx.i64_type()),
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => Ok(self.ctx.i32_type()),
        }
    }










}

#[cfg(test)]
mod devirt_tests {
    //! Soundness unit tests for the W18 closure-devirtualisation
    //! capture analysis. These exercise the IR-scan that decides which
    //! captures may be stamped `KnownClosure` (→ direct call) vs left as
    //! a genuinely-dynamic dispatch (→ runtime switch). Getting this
    //! wrong is a silent miscompile, so the analysis is pinned here
    //! independent of any end-to-end source.
    use super::*;
    use relon_ir::ir::{ClosureCapture, Func, IrType, Op, TaggedOp};
    use relon_parser::TokenRange;

    fn op(o: Op) -> TaggedOp {
        TaggedOp {
            op: o,
            range: TokenRange::default(),
        }
    }

    fn make_closure(fn_table_idx: u32, captures: Vec<ClosureCapture>) -> Op {
        let captures_size = captures.iter().map(|c| c.offset + 8).max().unwrap_or(0);
        Op::MakeClosure {
            fn_table_idx,
            captures,
            captures_size,
        }
    }

    fn cap(let_idx: u32, offset: u32) -> ClosureCapture {
        ClosureCapture {
            let_idx,
            ty: IrType::Closure,
            offset,
        }
    }

    fn entry_with_body(body: Vec<TaggedOp>) -> Func {
        Func {
            name: "run_main".into(),
            params: vec![IrType::I32],
            ret: IrType::I32,
            body,
            range: TokenRange::default(),
        }
    }

    /// A capture of a *known, non-self* closure is recorded so the
    /// capturing lambda's body can devirtualise the call against it.
    /// Mirrors the W18 predicate `(k) => is_prime(k, 2)` capturing the
    /// `is_prime` closure (`fn_table_idx=0`).
    #[test]
    fn records_known_non_self_capture() {
        // let0 := MakeClosure(K=0)  ; the `is_prime` binding
        // MakeClosure(L=1) capturing let0 at offset 0 ; the predicate
        let body = vec![
            op(make_closure(0, vec![cap(0, 0)])), // is_prime self-capture
            op(Op::LetSet {
                idx: 0,
                ty: IrType::Closure,
            }),
            op(make_closure(1, vec![cap(0, 0)])), // predicate captures is_prime
            op(Op::Call {
                fn_index: 14,
                arg_count: 2,
                param_tys: vec![IrType::ListInt, IrType::Closure],
                ret_ty: IrType::ListInt,
            }),
        ];
        let entry = entry_with_body(body);
        let table = build_known_capture_table(&entry, &[], &[]);
        // Lambda L=1 (the predicate) captures known closure K=0 at
        // offset 0.
        assert_eq!(
            table.get(&1).map(Vec::as_slice),
            Some(&[(0u32, 0u32)][..]),
            "predicate (L=1) must record its is_prime (K=0) capture as known"
        );
        // L=0 is_prime's own capture is a SELF capture (K==L==0) — it
        // must NOT appear here (the self-capture table owns it, and its
        // captures_ptr-reuse direct path is strictly better).
        assert!(
            !table.contains_key(&0),
            "self-capture (K==L) must be excluded from the known-capture table"
        );
    }

    /// When a closure let-slot is reassigned to a value that is NOT a
    /// literal `MakeClosure` (a genuinely-dynamic closure), the capture
    /// must NOT be recorded — the body keeps the runtime switch. This is
    /// the correctness red line: devirtualise only a provably-unique
    /// callee.
    #[test]
    fn drops_reassigned_dynamic_closure_slot() {
        // let0 := MakeClosure(0)        ; known
        // let0 := <some other Closure>  ; reassigned, now dynamic
        // MakeClosure(2) capturing let0 ; must NOT be recorded
        let body = vec![
            op(make_closure(0, vec![cap(0, 0)])),
            op(Op::LetSet {
                idx: 0,
                ty: IrType::Closure,
            }),
            // A bare `LetSet { Closure }` NOT preceded by a MakeClosure —
            // models a closure that arrived from somewhere unprovable
            // (a param, a phi, a different binding).
            op(Op::LetGet {
                idx: 5,
                ty: IrType::Closure,
            }),
            op(Op::LetSet {
                idx: 0,
                ty: IrType::Closure,
            }),
            op(make_closure(2, vec![cap(0, 0)])),
            op(Op::LetSet {
                idx: 9,
                ty: IrType::Closure,
            }),
        ];
        let entry = entry_with_body(body);
        let table = build_known_capture_table(&entry, &[], &[]);
        assert!(
            !table.contains_key(&2),
            "a capture of a reassigned (dynamic) closure slot must NOT be \
             recorded — the call must keep the runtime switch"
        );
    }

    /// The binding `LetSet` that immediately follows a known
    /// `MakeClosure` must NOT clear the slot it just established (the
    /// ordering bug fixed during development). A later capture of that
    /// slot is still recorded.
    #[test]
    fn binding_letset_does_not_clear_its_own_slot() {
        let body = vec![
            op(make_closure(3, vec![])),
            op(Op::LetSet {
                idx: 7,
                ty: IrType::Closure,
            }),
            op(make_closure(4, vec![cap(7, 0)])),
            op(Op::LetSet {
                idx: 8,
                ty: IrType::Closure,
            }),
        ];
        let entry = entry_with_body(body);
        let table = build_known_capture_table(&entry, &[], &[]);
        assert_eq!(
            table.get(&4).map(Vec::as_slice),
            Some(&[(0u32, 3u32)][..]),
            "L=4 must record its capture of known closure K=3 at offset 0"
        );
    }
}
