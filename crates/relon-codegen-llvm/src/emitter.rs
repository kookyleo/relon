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
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue,
};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};

use crate::error::LlvmError;
use crate::state::{
    ARENA_STATE_OFFSET_BASE, ARENA_STATE_OFFSET_SCRATCH_BASE, ARENA_STATE_OFFSET_SCRATCH_CURSOR,
    ARENA_STATE_OFFSET_TAIL_CURSOR,
};

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
        let bt = ir_ty_to_llvm_basic(ctx, *p).ok_or_else(|| {
            LlvmError::UnsupportedSignature(format!(
                "llvm-aot: helper `{}` param #{i} type {p:?} unsupported",
                func.name
            ))
        })?;
        param_types.push(basic_to_metadata(bt));
    }
    let ret_bt = ir_ty_to_llvm_basic(ctx, func.ret).ok_or_else(|| {
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
        let bt = ir_ty_to_llvm_basic(ctx, *p).ok_or_else(|| {
            LlvmError::UnsupportedSignature(format!(
                "llvm-aot: lambda `{}` param #{i} type {p:?} unsupported",
                func.name
            ))
        })?;
        param_types.push(basic_to_metadata(bt));
    }
    let ret_bt = ir_ty_to_llvm_basic(ctx, func.ret).ok_or_else(|| {
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

fn ir_ty_to_llvm_basic<'ctx>(ctx: &'ctx Context, ty: IrType) -> Option<BasicTypeEnum<'ctx>> {
    match ty {
        IrType::I64 => Some(ctx.i64_type().into()),
        IrType::I32 | IrType::Bool | IrType::Null => Some(ctx.i32_type().into()),
        IrType::F64 => Some(ctx.f64_type().into()),
        // Pointer-indirect leaves carry an i32 buffer-relative offset
        // (matches the cranelift `ir_ty_to_cl` widening). The IR-side
        // tag is preserved; the LLVM slot is plain i32.
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
/// [`IrType`]-shaped param / return mix that `ir_ty_to_llvm_basic`
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
struct Emit<'ctx, 'b, 'cp> {
    ctx: &'ctx Context,
    builder: &'b Builder<'ctx>,
    func: FunctionValue<'ctx>,
    /// Phase F.1: cached module reference so per-op lowering can
    /// declare extern symbols (the F.1 `str.contains` host shim) on
    /// demand without threading the module through every helper. The
    /// reference is borrowed for the emit pass only; `inkwell` keeps
    /// `Module` and `FunctionValue` lifetimes orthogonal so a borrow
    /// here doesn't conflict with the surrounding `add_function`
    /// calls in the entry/helper emit paths.
    module: &'b LlvmModule<'ctx>,
    shape: EntryShape,
    /// Cached `arena_base` pointer for the buffer-protocol entry.
    /// `None` for the legacy entry shape — `LoadField` / `StoreField`
    /// reject themselves before reaching for this value.
    arena_base_ptr: Option<PointerValue<'ctx>>,
    /// Cached state-pointer LLVM value (param 0 of the buffer entry).
    /// Phase E.1 uses it to load / store the per-call tail-cursor /
    /// scratch-cursor / scratch-base slots. `None` outside the
    /// buffer-protocol entry shape.
    state_ptr: Option<PointerValue<'ctx>>,
    /// Operand stack mirroring the IR's virtual stack. Every value
    /// in flight is an LLVM integer of the matching IR type. The
    /// pair tags the IR type so consumers can pick the right
    /// signed / unsigned predicate without re-deriving it.
    stack: Vec<TypedValue<'ctx>>,
    /// `LetSet { idx }` alloca slots, keyed by `(idx, ty)`. Each
    /// idx has at most one type at a time — the IR lowering pass
    /// guarantees no aliasing between idx's of different types.
    let_slots: std::collections::HashMap<u32, (PointerValue<'ctx>, IrType)>,
    /// LLVM param offset corresponding to `LocalGet(0)`. See
    /// [`Self::lookup_param`] — `param_base + idx` is the LLVM
    /// param index.
    param_base: u32,
    /// Label stack carrying the (entry_bb, exit_bb, kind) of every
    /// nested [`Op::Block`] / [`Op::Loop`]. `Br { label_depth }`
    /// indexes from the back (depth 0 = innermost). `Block`s exit
    /// to their tail; `Loop`s exit to their head.
    label_stack: Vec<LabelFrame<'ctx>>,
    /// Monotonic counter to mint unique LLVM basic block / value
    /// names so the dumped IR is human-readable.
    name_seq: u32,
    /// Phase B: hard-coded `return_root_size` returned from a
    /// buffer-protocol `Op::Return`. The IR producer leaves no
    /// value on the operand stack for `Return` under buffer
    /// protocol — the trampoline reads back `bytes_written` to
    /// decode the output record. We hard-code this to the schema's
    /// `return_layout.root_size`, passed in at emit time.
    buffer_return_size: u32,
    /// Phase D.1: set when emitting the fast-path entry. The
    /// `Op::LoadField` / `Op::StoreField` / `Op::Return` lowering
    /// branches consult this to rewrite the buffer-protocol IR
    /// against the typed `(i64...) -> i64` LLVM signature.
    fast_path: Option<FastEmit<'ctx>>,
    /// Phase E.2 multi-function lookup: when populated, `Op::Call`
    /// with `fn_index >= stdlib_function_count()` resolves to the
    /// matching sibling `FunctionValue` and emits a direct LLVM
    /// `call`. The map is keyed by IR-side `funcs` index (i.e.
    /// `fn_index - stdlib_count`). Empty for hand-built fixtures that
    /// never reference user-defined functions.
    helper_table: Option<HashMap<u32, FunctionValue<'ctx>>>,
    /// Phase E.2: when emitting a helper body (not the entry), this
    /// carries the IR-declared return type so `Op::Return` can pick
    /// the right LLVM `ret` shape. `None` while lowering the entry
    /// body — the entry's return shape is dictated by `EntryShape`.
    helper_ret_ty: Option<IrType>,
    /// Phase E.2: cached `llvm.trap` intrinsic `FunctionValue`. The
    /// intrinsic is declared once per module (in
    /// [`emit_module_funcs`]); each `Emit` snapshots the pointer so
    /// per-op `Div(I64)` / `Mod(I64)` guards can call it without
    /// re-querying the module.
    llvm_trap_fn: Option<FunctionValue<'ctx>>,
    /// Phase E.1: per-module const-data lookup. `Op::ConstString { idx }`
    /// reads the matching offset and pushes `iconst(I32, off)`.
    const_pool: &'cp ConstPool,
    /// Phase E.1: stack of inline call frames. `Op::Call` pushes one
    /// before lowering the callee body; `Op::Return` inside the
    /// callee body pops the typed value into the topmost frame's
    /// result alloca and jumps to its exit block. The callee's
    /// `LocalGet(idx)` resolves to `params[idx]` rather than the
    /// entry's LLVM params; `LetGet/LetSet` indices are remapped
    /// against `let_offset` so concurrent inline frames don't clash.
    inline_frames: Vec<InlineFrame<'ctx>>,
    /// Phase E.1: did the body emit a pointer-indirect StoreField?
    /// When set, the buffer-protocol epilogue returns the post-bump
    /// tail cursor (in bytes past `out_ptr`) rather than the
    /// statically-known `buffer_return_size`. Mirrors cranelift's
    /// `needs_tail_cursor` flag.
    needs_tail_cursor: bool,
    /// Phase F.W7: ordered list of lambda `FunctionValue`s, indexed by
    /// `fn_table_idx`. `Op::MakeClosure { fn_table_idx }` stamps the
    /// matching index into the closure handle's `fn_table_idx` slot
    /// and uses the same lookup to resolve the function pointer to
    /// stash. `Op::CallClosure` reads the handle's `fn_table_idx`
    /// slot and dispatches indirectly through a private global table
    /// of function pointers seeded from this list. Empty when the
    /// module contains no lambdas.
    closure_fn_table: Vec<FunctionValue<'ctx>>,
    /// Phase F.W7: per-IR-`record_local_idx` allocas backing
    /// `Op::AllocRootRecord` / `Op::StoreFieldAtRecord`. The slot
    /// holds an i32 out_ptr-relative offset; `AllocRootRecord` writes
    /// `0` there (root sits at `out_ptr + 0`), `StoreFieldAtRecord`
    /// reads it back to compute the destination address. Mirrors
    /// cranelift's `record_locals` map.
    record_locals: std::collections::HashMap<u32, PointerValue<'ctx>>,
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
    last_const_string: Option<Vec<u8>>,
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
    self_capture_offsets: Vec<(u32, u32)>,
    /// Phase F.W7 self-recursion fast path: let-slot indices that hold
    /// a self-recursive closure handle along with the enclosing
    /// lambda's `fn_table_idx`. Populated by `Op::LetSet` when the
    /// stored value carries [`Provenance::OwnCaptureHandle`] so the
    /// matching `Op::LetGet` can re-emit the provenance — this is what
    /// lets the recursive `fib(k - 1)` call site (which always goes
    /// through `LetGet`) keep the self-recursion fast path intact.
    self_capture_let_slots: std::collections::HashMap<u32, (u32, u32)>,
    /// Phase F.W7 self-recursion fast path: captures_ptr LLVM param
    /// (param 1) of the enclosing lambda. Cached so the closure-call
    /// emitter can pass it straight into the recursive call without
    /// re-loading from the closure handle. `None` when emitting the
    /// entry / a helper (not a lambda body) — the self-recursion fast
    /// path is gated on this being `Some`.
    captures_ptr_param: Option<IntValue<'ctx>>,
    /// Phase D.2 fast-path entry: let-slot indices holding a
    /// virtualised closure stamped by an in-body `Op::MakeClosure`
    /// (carries `Provenance::FastPathClosure`). The `LetSet` that
    /// catches such a value stashes the `fn_table_idx` here so the
    /// matching `LetGet` can re-emit the provenance, keeping the
    /// `CallClosure` direct-call rewrite alive across the let chain.
    /// Empty when not emitting the fast-path entry.
    fast_path_closure_let_slots: std::collections::HashMap<u32, u32>,
    /// Phase L W3: let-slot indices holding a `Provenance::ConstString`
    /// value (i.e. the let was set from a value sourced — directly or
    /// via prior `LetGet` chains — from an `Op::ConstString`). The
    /// matching `LetGet` re-stamps the provenance so the downstream
    /// `Op::Add(String)` lowering can switch to the const-len /
    /// single-byte-store fast path. Each entry records (len, optional
    /// first_byte). Empty by default; entries survive only across
    /// inner-loop iterations because the W3 reduce shape's `s` let is
    /// re-set every iteration from the same const literal.
    const_string_let_slots: std::collections::HashMap<u32, (u32, Option<u8>)>,
    /// Devirtualisation (W18): let-slot indices holding a real
    /// arena-resident closure handle whose `fn_table_idx` is a
    /// compile-time constant (`Provenance::KnownClosure`). The `LetSet`
    /// that catches such a value stashes the `fn_table_idx` here so the
    /// matching `LetGet` re-stamps the provenance, letting the downstream
    /// `CallClosure` emit a direct call (LLVM inlines it) instead of the
    /// runtime `switch i32 %cc_fn_idx`. A non-known-closure `LetSet`
    /// against the same slot wipes the entry so a later `LetGet` cannot
    /// fraudulently claim a static target. Empty by default.
    known_closure_let_slots: std::collections::HashMap<u32, u32>,
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
    known_capture_offsets: Vec<(u32, u32)>,
}

/// Phase E.1: per-call inline-frame state. One entry per active
/// stdlib `Op::Call`; the callee body lowers against the topmost
/// frame.
struct InlineFrame<'ctx> {
    /// LLVM values bound to the callee's `LocalGet(0..arity)` reads.
    /// Order matches the IR's declared parameter order — the
    /// `Op::Call` site popped them from the caller's operand stack
    /// (top-of-stack = last param) and reversed.
    params: Vec<TypedValue<'ctx>>,
    /// Offset added to the callee's `LetGet/LetSet` indices so its
    /// let-bindings don't alias the caller's slots. Mirrors the
    /// cranelift backend's `let_offset`.
    let_offset: u32,
    /// Result alloca + exit basic block. The callee's `Op::Return`
    /// stores the popped value into the alloca and unconditionally
    /// branches to `exit_bb`; the caller continues from there with a
    /// matching load.
    ret_slot: PointerValue<'ctx>,
    /// LLVM type stored at [`Self::ret_slot`]. Pre-computed from the
    /// IR-declared `ret_ty` of the stdlib call so the caller-side
    /// load knows what width to read.
    ret_ty: IrType,
    /// Branch target for `Op::Return` inside the callee body. The
    /// caller positions the builder here after the inline finishes
    /// and pushes the loaded return value back onto the operand
    /// stack.
    exit_bb: inkwell::basic_block::BasicBlock<'ctx>,
}

/// Phase D.1 fast-path emission state. Carried inside [`Emit`] when
/// lowering the typed fast entry.
#[derive(Clone)]
struct FastEmit<'ctx> {
    profile: FastPathProfile,
    /// Alloca holding the i64 return value. Trailing `StoreField`
    /// at `profile.ret_offset` writes into this slot; `Op::Return`
    /// loads from it.
    ret_slot: PointerValue<'ctx>,
}

#[derive(Clone, Copy)]
struct TypedValue<'ctx> {
    val: IntValue<'ctx>,
    /// IR-level tag of `val`. Recorded so Phase C predicates that
    /// inspect operand types (signed-vs-unsigned cmp, F64 routing)
    /// have it on hand without re-deriving from LLVM bit width.
    /// Phase B never consumes this field; `#[allow(dead_code)]`
    /// keeps the lint clean while we're still wiring future Op
    /// support.
    #[allow(dead_code)]
    ty: IrType,
    /// Provenance hint used by [`Emit::emit_call_closure`] to detect
    /// self-recursive closure calls. Defaults to [`Provenance::None`]
    /// for every push that doesn't go through the lambda-prologue
    /// capture path; the closure-self-call fast path only fires when
    /// the consumed handle's provenance points at one of the lambda's
    /// own self-capture offsets.
    prov: Provenance,
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
enum Provenance {
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
enum LabelKind {
    /// `Br` jumps **past** the block (forward exit).
    Block,
    /// `Br` jumps **back** to the loop header (continue).
    Loop,
}

#[derive(Clone)]
struct LabelFrame<'ctx> {
    /// Header basic block. For Block this is unused for branching
    /// (we never branch backward to the start of a block); for Loop
    /// it's the target of a `Br` (continue).
    header_bb: inkwell::basic_block::BasicBlock<'ctx>,
    /// Tail basic block — what code after the block / after the
    /// loop falls through to. For Block this is the `Br` target;
    /// for Loop the surrounding code lives here.
    tail_bb: inkwell::basic_block::BasicBlock<'ctx>,
    kind: LabelKind,
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    #[allow(clippy::too_many_arguments)]
    fn new(
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

    fn next_name(&mut self, hint: &str) -> String {
        self.name_seq += 1;
        format!("{hint}_{}", self.name_seq)
    }

    // -- stack helpers --------------------------------------------------

    fn push(&mut self, v: IntValue<'ctx>, ty: IrType) {
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
    fn push_with_prov(&mut self, v: IntValue<'ctx>, ty: IrType, prov: Provenance) {
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
    fn peek_self_capture_provenance(&self, offset: u32) -> Option<Provenance> {
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

    fn pop(&mut self, ip_hint: &str) -> Result<TypedValue<'ctx>, LlvmError> {
        self.stack.pop().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "operand stack underflow at {ip_hint}: producer emitted an Op with no matching push"
            ))
        })
    }

    fn pop_int(&mut self, ip_hint: &str) -> Result<IntValue<'ctx>, LlvmError> {
        self.pop(ip_hint).map(|tv| tv.val)
    }

    // -- locals / lets --------------------------------------------------

    fn lookup_param(&self, idx: u32) -> Result<IntValue<'ctx>, LlvmError> {
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

    fn ensure_let_slot(&mut self, idx: u32, ty: IrType) -> Result<PointerValue<'ctx>, LlvmError> {
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

    fn lower_body(&mut self, body: &[TaggedOp]) -> Result<(), LlvmError> {
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

    fn lower_op(&mut self, ip: usize, tagged: &TaggedOp) -> Result<(), LlvmError> {
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

            other => {
                return Err(LlvmError::Codegen(format!(
                    "unsupported op (Phase E.1 envelope): {other:?} at ip={ip}"
                )));
            }
        }
        Ok(())
    }

    // -- Phase E.1: inline-call frame helpers --------------------------

    /// Translate a callee `LetGet/LetSet` index against the topmost
    /// inline frame. Mirrors cranelift's `remap_let_idx`.
    fn remap_let_idx(&self, idx: u32) -> u32 {
        match self.inline_frames.last() {
            Some(frame) => frame.let_offset.saturating_add(idx),
            None => idx,
        }
    }

    /// Lower `Op::Return`. The shape decides what flows back:
    ///
    /// - Legacy-i64: pop the top of the operand stack and `ret v`.
    /// - Buffer-protocol: return a hard-coded i32 `return_root_size`
    ///   so the host trampoline reads back the full fixed area.
    ///   Phase B doesn't emit pointer-indirect StoreField, so the
    ///   tail-cursor path is dead — `return_root_size` is enough.
    ///
    /// Mirrors the cranelift backend's `emit_return` for the same
    /// shapes.
    fn emit_return(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        // Phase E.1: inline-frame return. The callee body pops the
        // typed return value, stores it into the frame's ret_slot,
        // then unconditionally jumps to exit_bb. The caller side picks
        // up from there in `emit_call_stdlib`.
        if let Some((ret_slot, exit_bb, ret_ty)) = self
            .inline_frames
            .last()
            .map(|f| (f.ret_slot, f.exit_bb, f.ret_ty))
        {
            let v = self.pop(ip_hint)?;
            // Coerce the popped value's width to the slot type if
            // needed (Bool / Null on an i32 stack but stored as i32
            // already — no coercion. String / ListInt on i32 — same.
            // I64 on i64 — same. We rely on the caller's typing
            // contract.)
            let stored = self.coerce_to_let_ty(v, ret_ty)?;
            self.builder
                .build_store(ret_slot, stored)
                .map_err(|e| LlvmError::Codegen(format!("inline Return store: {e}")))?;
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline Return br: {e}")))?;
            // Open a fresh dummy block so any subsequent ops the body
            // emits (e.g. dead trailing ConstBool after Trap) have
            // somewhere to land. LLVM's verifier prunes the dead chain.
            let dummy = self.ctx.append_basic_block(self.func, "after_inline_ret");
            self.builder.position_at_end(dummy);
            return Ok(());
        }
        // Phase D.1 fast path: the trailing buffer-protocol `Op::Return`
        // doesn't carry a value on the stack (the IR producer already
        // emitted a `StoreField` into the output buffer that the fast
        // emitter redirected into `ret_slot`). Load + `ret` from the
        // slot to produce the typed i64 result.
        if let Some(fast) = self.fast_path.as_ref() {
            let i64_t = self.ctx.i64_type();
            let v = self
                .builder
                .build_load(i64_t, fast.ret_slot, "fast_ret_load")
                .map_err(|e| LlvmError::Codegen(format!("fast Return load: {e}")))?
                .into_int_value();
            self.builder
                .build_return(Some(&v))
                .map_err(|e| LlvmError::Codegen(format!("fast Return: {e}")))?;
            // Open a dead continuation block so downstream ops have
            // somewhere to land — matches the buffer/legacy branches
            // below. The block stays dead; the verifier accepts it
            // once we seal with `unreachable` in `lower_body`'s
            // trailing branch.
            let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
            self.builder.position_at_end(cont);
            // Suppress the `_` warning on ip_hint when this branch
            // runs.
            let _ = ip_hint;
            return Ok(());
        }
        // Phase E.2 helper-body return: when lowering a sibling
        // function rather than the entry, pop the operand and emit a
        // typed return matching the helper's declared IR return type.
        // Widens / truncates the popped i32 / i64 to the declared LLVM
        // ret slot when the two widths disagree.
        if let Some(ret_ty) = self.helper_ret_ty {
            let v = self.pop_int(ip_hint)?;
            let want_width = match ret_ty {
                IrType::I64 => 64,
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
                IrType::F64 => {
                    return Err(LlvmError::Codegen(
                        "helper Return: F64 not yet supported in Phase E.2".into(),
                    ));
                }
            };
            let have_width = v.get_type().get_bit_width();
            let final_v = if have_width == want_width {
                v
            } else if have_width < want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                self.builder
                    .build_int_z_extend(v, target_ty, "helper_ret_zext")
                    .map_err(|e| LlvmError::Codegen(format!("helper Return zext: {e}")))?
            } else {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                self.builder
                    .build_int_truncate(v, target_ty, "helper_ret_trunc")
                    .map_err(|e| LlvmError::Codegen(format!("helper Return trunc: {e}")))?
            };
            self.builder
                .build_return(Some(&final_v))
                .map_err(|e| LlvmError::Codegen(format!("helper Return: {e}")))?;
            let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
            self.builder.position_at_end(cont);
            return Ok(());
        }
        match self.shape {
            EntryShape::LegacyI64 => {
                let v = self.pop_int(ip_hint)?;
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return (legacy): {e}")))?;
            }
            EntryShape::Buffer => {
                let i32_t = self.ctx.i32_type();
                // Phase E.1: when the body emitted a pointer-indirect
                // StoreField (String / List* return) the trampoline
                // needs to know how many bytes past `out_ptr` the tail
                // cursor advanced to. Read it back from the state slot
                // so the host can decode the variable-length payload.
                // Bodies that only wrote into the fixed area keep the
                // historical "return root_size" path so a trampoline
                // that doesn't bother to consult `tail_cursor` still
                // works.
                let v: IntValue<'ctx> = if self.needs_tail_cursor {
                    let state_ptr = self.state_ptr.ok_or_else(|| {
                        LlvmError::Codegen(
                            "buffer Return needs tail_cursor but state ptr unavailable".into(),
                        )
                    })?;
                    let i8_t = self.ctx.i8_type();
                    let tail_gep = unsafe {
                        self.builder
                            .build_in_bounds_gep(
                                i8_t,
                                state_ptr,
                                &[i32_t
                                    .const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                                "tail_cursor_gep",
                            )
                            .map_err(|e| LlvmError::Codegen(format!("tail_cursor GEP: {e}")))?
                    };
                    self.builder
                        .build_load(i32_t, tail_gep, "tail_cursor")
                        .map_err(|e| LlvmError::Codegen(format!("tail_cursor load: {e}")))?
                        .into_int_value()
                } else {
                    i32_t.const_int(u64::from(self.buffer_return_size), false)
                };
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return (buffer): {e}")))?;
            }
        }
        // After the explicit return, the rest of the surrounding
        // body is unreachable. Open a fresh continuation block so
        // any subsequent ops (a stray `LetGet` after a Br-tail
        // Return, etc.) emit somewhere valid. The block is dead;
        // LLVM's verifier accepts it as long as it ends with a
        // terminator — we seal it with `unreachable` lazily when
        // the next terminator-emitting op needs to bind it.
        let cont = self.ctx.append_basic_block(self.func, "after_return_cont");
        self.builder.position_at_end(cont);
        Ok(())
    }

    /// Phase E.2 multi-function dispatch: lower `Op::Call`.
    ///
    /// The IR's `fn_index` is split as `[0..stdlib_count) = bundled
    /// stdlib body` / `[stdlib_count..) = user-defined sibling`. The
    /// LLVM emitter currently only routes the sibling slice — stdlib
    /// inlining stays parked on the cranelift backend. A stdlib call
    /// surfaces `LlvmError::Codegen` so the host can fall back.
    fn emit_call(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib_count = relon_ir::stdlib::stdlib_function_count();
        if fn_index < stdlib_count {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to stdlib fn_index={fn_index} not yet supported in LLVM AOT \
                 (cranelift inlines bundled stdlib bodies; LLVM path widens with #278)"
            )));
        }
        let helper_idx = fn_index - stdlib_count;
        let callee = match self.helper_table.as_ref().and_then(|t| t.get(&helper_idx)) {
            Some(fv) => *fv,
            None => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx} (fn_index={fn_index}, stdlib_count={stdlib_count}) \
                     not in helper_table — module may be missing the function"
                )));
            }
        };

        // Sanity check arity against the declared signature.
        if callee.count_params() as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: callee has {} LLVM params, IR declares {}",
                callee.count_params(),
                param_tys.len()
            )));
        }
        if arg_count as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: arg_count={arg_count} != param_tys.len()={}",
                param_tys.len()
            )));
        }

        // Pop the arguments off the operand stack — last-pushed value
        // is the last param.
        let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop_int(ip_hint)?.into());
        }
        args.reverse();

        // Adjust each arg's LLVM type to match the callee's declared
        // param: widen / truncate i32 <-> i64 as needed. The IR's
        // stack-machine semantics keep types tagged but the wasm slot
        // widening can leave a Bool-as-i32 in front of an I64 callee
        // param. We re-coerce here to match the helper's signature.
        for (i, (slot, want_ty)) in args.iter_mut().zip(param_tys.iter()).enumerate() {
            let arg_val = match slot {
                BasicMetadataValueEnum::IntValue(v) => *v,
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: expected IntValue, got {other:?}"
                    )));
                }
            };
            let want_width = match *want_ty {
                IrType::I64 => 64,
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
                IrType::F64 => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: F64 param not yet supported in Phase E.2"
                    )));
                }
            };
            let have_width = arg_val.get_type().get_bit_width();
            if have_width != want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                let coerced = if have_width < want_width {
                    self.builder
                        .build_int_z_extend(arg_val, target_ty, "call_arg_zext")
                        .map_err(|e| LlvmError::Codegen(format!("call arg zext: {e}")))?
                } else {
                    self.builder
                        .build_int_truncate(arg_val, target_ty, "call_arg_trunc")
                        .map_err(|e| LlvmError::Codegen(format!("call arg trunc: {e}")))?
                };
                *slot = coerced.into();
            }
        }

        let name = self.next_name("call_ret");
        let call_site = self
            .builder
            .build_call(callee, &args, &name)
            .map_err(|e| LlvmError::Codegen(format!("Op::Call build_call: {e}")))?;
        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned void; Phase E.2 envelope expects a typed return"
                )));
            }
        };
        let ret_int = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned {other:?}, expected IntValue"
                )));
            }
        };
        self.push(ret_int, ret_ty);
        Ok(())
    }

    // -- helpers --------------------------------------------------------

    fn coerce_to_let_ty(
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

    fn emit_binop(&mut self, ip_hint: &str, ty: IrType, op: BinOp) -> Result<(), LlvmError> {
        let b = self.pop_int(ip_hint)?;
        let a = self.pop_int(ip_hint)?;

        // AOT-1 scalar Float slice: the operand stack carries f64 as
        // i64 bits tagged `IrType::F64`. Compute the arithmetic in the
        // float domain by bit-casting both operands to `double`, then
        // bit-cast the result back to i64 bits before pushing. The IR
        // guarantees homogeneous F64 operands (lowering rejects
        // mixed Int/Float), so no int<->float promotion is needed.
        //
        // The integer div-by-zero trap guard below assumes integer
        // operands (`build_int_compare` against an integer zero), so the
        // F64 path runs its own guard inside `emit_binop_f64`. The float
        // guard matches the tree-walker oracle, which raises
        // `DivisionByZero` for `x / 0.0` (see
        // `relon-evaluator::arithmetic::eval_numeric_division`) rather
        // than yielding IEEE ±inf.
        if ty == IrType::F64 {
            return self.emit_binop_f64(op, a, b);
        }

        // Phase E.2 sandbox parity: guard Div / Mod against a zero RHS
        // so the JIT raises a deterministic trap instead of leaving
        // LLVM's `sdiv` / `srem` to invoke UB (which on x86 surfaces
        // as a host-level SIGFPE that the host can't catch on stable
        // Rust). Emit an `if rhs == 0 { llvm.trap; unreachable } else
        // { ... }` skeleton and continue the division in the `else`
        // arm. The `unreachable` after `llvm.trap` is what tells LLVM
        // the trap path doesn't fall through.
        if matches!(op, BinOp::Div | BinOp::Mod) {
            let zero = b.get_type().const_zero();
            let cmp_name = self.next_name("divz_cmp");
            let is_zero = self
                .builder
                .build_int_compare(IntPredicate::EQ, b, zero, &cmp_name)
                .map_err(|e| LlvmError::Codegen(format!("{} divz cmp: {e}", op.name())))?;
            let trap_bb = self.ctx.append_basic_block(self.func, "div_by_zero_trap");
            let cont_bb = self.ctx.append_basic_block(self.func, "div_by_zero_ok");
            self.builder
                .build_conditional_branch(is_zero, trap_bb, cont_bb)
                .map_err(|e| LlvmError::Codegen(format!("{} divz branch: {e}", op.name())))?;
            // Trap block: call `llvm.trap` then `unreachable`. The
            // intrinsic is declared lazily; subsequent emits reuse the
            // declaration so the module ends up with at most one
            // `@llvm.trap` symbol regardless of how many guards fire.
            self.builder.position_at_end(trap_bb);
            self.emit_llvm_trap_call(op.name())?;
            self.builder
                .build_unreachable()
                .map_err(|e| LlvmError::Codegen(format!("{} divz unreachable: {e}", op.name())))?;
            // Continue normal codegen in the "ok" block.
            self.builder.position_at_end(cont_bb);
        }

        let name = self.next_name(op.name());
        let r = match op {
            BinOp::Add => self.builder.build_int_add(a, b, &name),
            BinOp::Sub => self.builder.build_int_sub(a, b, &name),
            BinOp::Mul => self.builder.build_int_mul(a, b, &name),
            BinOp::Div => self.builder.build_int_signed_div(a, b, &name),
            BinOp::Mod => self.builder.build_int_signed_rem(a, b, &name),
            BinOp::BitAnd => self.builder.build_and(a, b, &name),
        }
        .map_err(|e| LlvmError::Codegen(format!("{} build failed: {e}", op.name())))?;
        self.push(r, ty);
        Ok(())
    }

    /// AOT-1: lower an `F64` binary op. `a` / `b` are the operand-stack
    /// i64 bit patterns; we bit-cast to `double`, run the matching
    /// `build_float_*`, and bit-cast the result back to i64 bits so the
    /// virtual stack stays integer-typed (Option B — no enum StackVal
    /// rewrite). `Mod` has no float analogue and is rejected here
    /// (lowering already declines `%` on F64, so this is defence in
    /// depth rather than a reachable path).
    ///
    /// `Div` carries a float-zero trap guard: the tree-walker oracle
    /// raises `DivisionByZero` whenever the divisor compares equal to
    /// `0.0` (which `OEQ` matches for both `+0.0` and `-0.0`, and
    /// declines for `NaN`), so the JIT must trap on the same operands
    /// rather than producing IEEE ±inf.
    fn emit_binop_f64(
        &mut self,
        op: BinOp,
        a: IntValue<'ctx>,
        b: IntValue<'ctx>,
    ) -> Result<(), LlvmError> {
        let f64_t = self.ctx.f64_type();
        let af = self
            .builder
            .build_bit_cast(a, f64_t, &self.next_name("fbin_a"))
            .map_err(|e| LlvmError::Codegen(format!("{} f64 lhs bitcast: {e}", op.name())))?
            .into_float_value();
        let bf = self
            .builder
            .build_bit_cast(b, f64_t, &self.next_name("fbin_b"))
            .map_err(|e| LlvmError::Codegen(format!("{} f64 rhs bitcast: {e}", op.name())))?
            .into_float_value();
        if matches!(op, BinOp::Div) {
            let zero = f64_t.const_zero();
            let cmp_name = self.next_name("fdivz_cmp");
            let is_zero = self
                .builder
                .build_float_compare(FloatPredicate::OEQ, bf, zero, &cmp_name)
                .map_err(|e| LlvmError::Codegen(format!("f64 divz cmp: {e}")))?;
            let trap_bb = self.ctx.append_basic_block(self.func, "fdiv_by_zero_trap");
            let cont_bb = self.ctx.append_basic_block(self.func, "fdiv_by_zero_ok");
            self.builder
                .build_conditional_branch(is_zero, trap_bb, cont_bb)
                .map_err(|e| LlvmError::Codegen(format!("f64 divz branch: {e}")))?;
            self.builder.position_at_end(trap_bb);
            self.emit_llvm_trap_call("fdiv")?;
            self.builder
                .build_unreachable()
                .map_err(|e| LlvmError::Codegen(format!("f64 divz unreachable: {e}")))?;
            self.builder.position_at_end(cont_bb);
        }
        let name = self.next_name(op.name());
        let rf = match op {
            BinOp::Add => self.builder.build_float_add(af, bf, &name),
            BinOp::Sub => self.builder.build_float_sub(af, bf, &name),
            BinOp::Mul => self.builder.build_float_mul(af, bf, &name),
            BinOp::Div => self.builder.build_float_div(af, bf, &name),
            BinOp::Mod | BinOp::BitAnd => {
                return Err(LlvmError::Codegen(format!(
                    "{} not defined for F64 operands",
                    op.name()
                )));
            }
        }
        .map_err(|e| LlvmError::Codegen(format!("{} f64 build failed: {e}", op.name())))?;
        let bits = self
            .builder
            .build_bit_cast(rf, self.ctx.i64_type(), &self.next_name("fbin_bits"))
            .map_err(|e| LlvmError::Codegen(format!("{} f64 result bitcast: {e}", op.name())))?
            .into_int_value();
        self.push(bits, IrType::F64);
        Ok(())
    }

    /// Phase E.2: emit a call to the `llvm.trap` intrinsic. The
    /// intrinsic must be pre-declared on the module via
    /// [`declare_llvm_trap`] before the first guard fires; the
    /// declaration is cached on the `Emit` so repeated div / mod
    /// guards share one `FunctionValue`. The `op_hint` is used only
    /// for diagnostic naming on the build_call site.
    fn emit_llvm_trap_call(&mut self, op_hint: &str) -> Result<(), LlvmError> {
        let trap_fn = self.llvm_trap_fn.ok_or_else(|| {
            LlvmError::Codegen(format!(
                "{op_hint}: llvm.trap intrinsic missing — emit_module_funcs forgot to declare it"
            ))
        })?;
        let name = self.next_name("trap_call");
        self.builder
            .build_call(trap_fn, &[], &name)
            .map_err(|e| LlvmError::Codegen(format!("{op_hint} llvm.trap build_call: {e}")))?;
        Ok(())
    }

    fn emit_cmp(
        &mut self,
        ip_hint: &str,
        operand_ty: IrType,
        pred: IntPredicate,
    ) -> Result<(), LlvmError> {
        // Pop in the order [b, a] — the deepest operand is the first
        // push (lhs of the comparison).
        let b = self.pop_int(ip_hint)?;
        let a = self.pop_int(ip_hint)?;
        // AOT-1 scalar Float slice: an F64 comparison reinterprets the
        // i64-bits operands as `double`. The predicate choice tracks the
        // tree-walker oracle exactly, NOT raw IEEE:
        //
        // * Ordering (`< <= > >=`) routes to the ORDERED predicates
        //   (OLT/OLE/OGT/OGE). These are false when either operand is
        //   NaN, matching the evaluator's `eval_numeric_comparison`
        //   (Rust native `f64` `<` etc.).
        // * Equality (`==` / `!=`) follows `Value`'s `PartialEq`, which
        //   compares `Value::Float` through `OrderedFloat`: `NaN == NaN`
        //   is *true* and `-0.0 == 0.0` is *true*. IEEE `OEQ` gets the
        //   zero case right but says `NaN == NaN` is false, so we OR in
        //   an explicit both-NaN test: `eq = OEQ(a,b) | (isnan(a) &
        //   isnan(b))`, and `ne = !eq`.
        let result_i1 = if operand_ty == IrType::F64 {
            let f64_t = self.ctx.f64_type();
            let af = self
                .builder
                .build_bit_cast(a, f64_t, &self.next_name("fcmp_a"))
                .map_err(|e| LlvmError::Codegen(format!("Cmp f64 lhs bitcast: {e}")))?
                .into_float_value();
            let bf = self
                .builder
                .build_bit_cast(b, f64_t, &self.next_name("fcmp_b"))
                .map_err(|e| LlvmError::Codegen(format!("Cmp f64 rhs bitcast: {e}")))?
                .into_float_value();
            match pred {
                IntPredicate::EQ | IntPredicate::NE => {
                    let oeq = self
                        .builder
                        .build_float_compare(FloatPredicate::OEQ, af, bf, &self.next_name("foeq"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 oeq: {e}")))?;
                    // `UNO(x, x)` is true iff `x` is NaN (the only way a
                    // value is unordered with itself).
                    let a_nan = self
                        .builder
                        .build_float_compare(FloatPredicate::UNO, af, af, &self.next_name("fanan"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 lhs isnan: {e}")))?;
                    let b_nan = self
                        .builder
                        .build_float_compare(FloatPredicate::UNO, bf, bf, &self.next_name("fbnan"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 rhs isnan: {e}")))?;
                    let both_nan = self
                        .builder
                        .build_and(a_nan, b_nan, &self.next_name("fbothnan"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 both-nan and: {e}")))?;
                    let eq = self
                        .builder
                        .build_or(oeq, both_nan, &self.next_name("feq"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 eq or: {e}")))?;
                    if matches!(pred, IntPredicate::EQ) {
                        eq
                    } else {
                        self.builder
                            .build_not(eq, &self.next_name("fne"))
                            .map_err(|e| LlvmError::Codegen(format!("Cmp f64 ne not: {e}")))?
                    }
                }
                ord => {
                    let fpred = match ord {
                        IntPredicate::SLT => FloatPredicate::OLT,
                        IntPredicate::SLE => FloatPredicate::OLE,
                        IntPredicate::SGT => FloatPredicate::OGT,
                        IntPredicate::SGE => FloatPredicate::OGE,
                        other => {
                            return Err(LlvmError::Codegen(format!(
                                "Cmp f64: unsupported predicate {other:?}"
                            )));
                        }
                    };
                    let name = self.next_name("fcmp");
                    self.builder
                        .build_float_compare(fpred, af, bf, &name)
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 build failed: {e}")))?
                }
            }
        } else {
            // Phase B keeps every integer comparison signed (matches
            // what the IR producer emits for `Lt` / `Le` / `Gt` / `Ge`).
            // `Eq` / `Ne` are signedness-agnostic at the LLVM level, so
            // the producer's predicate flows through unchanged.
            let name = self.next_name("cmp");
            self.builder
                .build_int_compare(pred, a, b, &name)
                .map_err(|e| LlvmError::Codegen(format!("Cmp build failed: {e}")))?
        };
        // The IR's virtual stack wants a `Bool` (i32 slot). Widen the
        // i1 to i32 so the rest of the pipeline (StoreField for Bool
        // returns, BrIf for control flow) sees the canonical width.
        let name_zext = self.next_name("cmp_zext");
        let widened = self
            .builder
            .build_int_z_extend(result_i1, self.ctx.i32_type(), &name_zext)
            .map_err(|e| LlvmError::Codegen(format!("Cmp zext: {e}")))?;
        self.push(widened, IrType::Bool);
        Ok(())
    }

    /// Emit a LoadField — buffer-protocol only. The LLVM IR loads
    /// `arena_base + in_ptr + offset` for a value of `ty`. Phase D.1
    /// fast-path mode short-circuits this into a direct LLVM param
    /// access against the matching arg slot.
    fn emit_load_field(&mut self, offset: u32, ty: IrType) -> Result<(), LlvmError> {
        // Phase D.1 fast path: lift the buffer-protocol field load
        // into a direct LLVM param read whenever the field's offset
        // matches one of the profile's declared arg offsets.
        if let Some(fast) = self.fast_path.as_ref() {
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path LoadField: only I64 args supported, got {ty:?}"
                )));
            }
            let slot = fast
                .profile
                .arg_offsets
                .iter()
                .position(|&o| o == offset)
                .ok_or_else(|| {
                    LlvmError::Codegen(format!(
                        "fast-path LoadField: offset {offset} not in profile.arg_offsets"
                    ))
                })?;
            // LLVM param `slot` is the i64 arg directly under the
            // fast-entry signature (no implicit state slot, no
            // handshake i32 quartet).
            let p = self.func.get_nth_param(slot as u32).ok_or_else(|| {
                LlvmError::Codegen(format!(
                    "fast-path LoadField: llvm param #{slot} missing on function"
                ))
            })?;
            let v = p.into_int_value();
            self.push(v, IrType::I64);
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("LoadField outside buffer-protocol entry shape".into())
        })?;
        let in_ptr_i32 = self.lookup_param(0)?; // IR LocalGet(0) == in_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, in_ptr_i32, offset)?;
        // AOT-1: an F64 field is stored as 8 LE bytes; load it as a
        // `double`, then bit-cast to i64 bits so the operand stack stays
        // integer-typed (Option B). Routing it through `field_load_kind`
        // would yield a `FloatValue` that the shared `.into_int_value()`
        // tail below cannot consume.
        if ty == IrType::F64 {
            let name = self.next_name("loadf_f64");
            let f = self
                .builder
                .build_load(self.ctx.f64_type(), addr, &name)
                .map_err(|e| LlvmError::Codegen(format!("LoadField f64 load: {e}")))?;
            let bits = self
                .builder
                .build_bit_cast(f, self.ctx.i64_type(), &self.next_name("loadf_f64_bits"))
                .map_err(|e| LlvmError::Codegen(format!("LoadField f64 bitcast: {e}")))?
                .into_int_value();
            self.push(bits, IrType::F64);
            return Ok(());
        }
        let (llvm_ty, push_ty) = self.field_load_kind(ty)?;
        let name = self.next_name("loadf");
        let raw = self
            .builder
            .build_load(llvm_ty, addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("LoadField load: {e}")))?
            .into_int_value();
        // Widen Bool / Null (i8 on the wire) to i32 to match the IR's
        // virtual-stack convention; I32 / I64 / I8-tagged-as-Null are
        // already the correct width.
        let widened = match push_ty {
            IrType::Bool | IrType::Null => {
                let name = self.next_name("loadf_zext");
                self.builder
                    .build_int_z_extend(raw, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadField zext: {e}")))?
            }
            _ => raw,
        };
        self.push(widened, push_ty);
        Ok(())
    }

    fn emit_store_field(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        // Phase E.1: pointer-indirect types (String / List*) route to
        // the tail-cursor protocol — bump-allocate inside the output
        // buffer's tail region, memcpy the record there, and stamp
        // the buffer-relative offset into the fixed-area slot. Comes
        // before the Phase D.1 fast-path check because the fast path
        // explicitly rejects non-I64 stores.
        if matches!(
            ty,
            IrType::String | IrType::ListInt | IrType::ListFloat | IrType::ListBool
        ) {
            return self.emit_store_field_pointer_indirect(ip_hint, offset, ty);
        }
        // Phase D.1 fast path: rewrite trailing StoreField into a
        // store against the i64 ret_slot. Only the single Int return
        // slot is supported — any other offset means the IR is past
        // the fast-path envelope (multi-field record, tail-cursor
        // payload) and we reject.
        if let Some(fast) = self.fast_path.clone() {
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreField: only I64 returns supported, got {ty:?}"
                )));
            }
            if offset != fast.profile.ret_offset {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreField: offset {offset} != profile.ret_offset {}",
                    fast.profile.ret_offset
                )));
            }
            let v = self.pop_int(ip_hint)?;
            self.builder
                .build_store(fast.ret_slot, v)
                .map_err(|e| LlvmError::Codegen(format!("fast StoreField ret_slot: {e}")))?;
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField outside buffer-protocol entry shape".into())
        })?;
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, out_ptr_i32, offset)?;
        let v = self.pop_int(ip_hint)?;
        let store_val: BasicValueEnum<'ctx> = match ty {
            IrType::I64 => v.into(),
            IrType::I32 => v.into(),
            IrType::F64 => {
                // The IR's virtual stack carries f64 as bit-cast i64;
                // we don't see ConstF64 / Add(F64) in the Phase B
                // envelope, but a future LoadField -> StoreField pair
                // could leave an i64 on the stack tagged as F64.
                // Treat it as an i64 store; the bit-cast happens at
                // the host side.
                v.into()
            }
            IrType::Bool | IrType::Null => {
                // Narrow the i32 to i8 before storing.
                let name = self.next_name("storef_trunc");
                let narrowed = self
                    .builder
                    .build_int_truncate(v, self.ctx.i8_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreField trunc: {e}")))?;
                narrowed.into()
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "StoreField: Phase B envelope rejects {other:?}"
                )));
            }
        };
        self.builder
            .build_store(addr, store_val)
            .map_err(|e| LlvmError::Codegen(format!("StoreField store: {e}")))?;
        Ok(())
    }

    /// Compute `arena_base + buf_ptr + offset` as an LLVM pointer.
    /// The result is a typed-stripped opaque pointer suitable for any
    /// `load` / `store` width.
    fn compute_buffer_addr(
        &mut self,
        arena_base_ptr: PointerValue<'ctx>,
        buf_ptr_i32: IntValue<'ctx>,
        offset: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        // Widen `buf_ptr_i32` to i64 (zero-extend — wasm semantics
        // treat the i32 as an unsigned byte offset).
        let name = self.next_name("buf_ptr_zext");
        let buf_ptr64 = self
            .builder
            .build_int_z_extend(buf_ptr_i32, i64_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("buf_ptr zext: {e}")))?;
        let off_const = i32_t.const_int(u64::from(offset), false);
        let off64 = self
            .builder
            .build_int_z_extend(off_const, i64_t, "off_zext")
            .map_err(|e| LlvmError::Codegen(format!("offset zext: {e}")))?;
        let name = self.next_name("buf_off");
        let combined = self
            .builder
            .build_int_add(buf_ptr64, off64, &name)
            .map_err(|e| LlvmError::Codegen(format!("buf_ptr + offset: {e}")))?;
        // GEP from the cached arena_base pointer (which is an i8*)
        // by the combined byte offset.
        let name = self.next_name("field_addr");
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[combined], &name)
                .map_err(|e| LlvmError::Codegen(format!("field GEP: {e}")))?
        };
        Ok(addr)
    }

    // -- control flow ---------------------------------------------------

    fn emit_block(
        &mut self,
        result_ty: Option<IrType>,
        body: &[TaggedOp],
    ) -> Result<(), LlvmError> {
        if result_ty.is_some() {
            return Err(LlvmError::Codegen(
                "Block with result_ty: Phase B envelope does not carry block-result phis".into(),
            ));
        }
        let header_bb = self.ctx.append_basic_block(self.func, "block_head");
        let tail_bb = self.ctx.append_basic_block(self.func, "block_tail");

        // Fallthrough from the current insertion point into the
        // block's header.
        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| LlvmError::Codegen(format!("Block fallthrough: {e}")))?;
        self.builder.position_at_end(header_bb);

        self.label_stack.push(LabelFrame {
            header_bb,
            tail_bb,
            kind: LabelKind::Block,
        });
        // Devirtualisation (W18) correctness: a `Br` can exit the block
        // early, skipping a later `LetSet { Closure }`; a `LetGet` after
        // the block (the `Br` target) would then read a slot the emitter
        // believes holds a known closure (because emission walks the
        // body linearly) but a runtime early-exit path never set. Drop,
        // around the block, every closure slot the body reassigns — the
        // post-block `LetGet` then falls back to the runtime switch on
        // the early-exit path. Straight-line uses inside the block still
        // devirtualise.
        let mut body_closure_setslots: Vec<u32> = Vec::new();
        collect_closure_letset_slots(body, &mut body_closure_setslots);
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        // If the body ran without an explicit `Br`, fall through to
        // `tail_bb`. A `Br` that fired already terminated the current
        // block via `build_unconditional_branch`; in that case the
        // builder's current block is already terminated and we must
        // not emit another branch.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(tail_bb)
                .map_err(|e| LlvmError::Codegen(format!("Block tail fallthrough: {e}")))?;
        }
        self.builder.position_at_end(tail_bb);
        self.label_stack.pop();
        Ok(())
    }

    fn emit_loop(&mut self, result_ty: Option<IrType>, body: &[TaggedOp]) -> Result<(), LlvmError> {
        if result_ty.is_some() {
            return Err(LlvmError::Codegen(
                "Loop with result_ty: Phase B envelope does not carry loop-result phis".into(),
            ));
        }
        let header_bb = self.ctx.append_basic_block(self.func, "loop_head");
        let tail_bb = self.ctx.append_basic_block(self.func, "loop_tail");

        self.builder
            .build_unconditional_branch(header_bb)
            .map_err(|e| LlvmError::Codegen(format!("Loop fallthrough: {e}")))?;
        self.builder.position_at_end(header_bb);

        self.label_stack.push(LabelFrame {
            header_bb,
            tail_bb,
            kind: LabelKind::Loop,
        });
        // Devirtualisation (W18) correctness: a loop body runs 0+ times
        // and a `LetGet` at the top of the body re-executes every
        // iteration, so a `KnownClosure` let-slot the body *reassigns*
        // cannot be trusted on a path that reads it before that
        // reassignment ran (iteration 1's top, or after a 0-trip loop).
        // Conservatively drop, both before emitting the body and on loop
        // exit, every slot the body contains a `LetSet { Closure }` for
        // (at any nesting depth). A within-body `MakeClosure; LetSet`
        // still re-establishes the entry in source order for the reads
        // that follow it in the same iteration (its `fn_table_idx` is a
        // compile-time constant, identical every iteration), so
        // straight-line uses inside the body keep devirtualising; only
        // cross-iteration / loop-carried reads fall back to the switch.
        // W18's filter loop reads its predicate via the inline-frame
        // param (not a body-bound let), so it is unaffected.
        let mut body_closure_setslots: Vec<u32> = Vec::new();
        collect_closure_letset_slots(body, &mut body_closure_setslots);
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        for (ip, tagged) in body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        for s in &body_closure_setslots {
            self.known_closure_let_slots.remove(&self.remap_let_idx(*s));
        }
        // If the body fell through without an explicit `Br`, that's
        // an implicit "exit the loop" in wasm semantics — the loop
        // body executed once and the loop terminates. Emit a branch
        // to `tail_bb`.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(tail_bb)
                .map_err(|e| LlvmError::Codegen(format!("Loop implicit exit: {e}")))?;
        }
        self.builder.position_at_end(tail_bb);
        self.label_stack.pop();
        Ok(())
    }

    fn label_target(&self, depth: u32) -> Result<&LabelFrame<'ctx>, LlvmError> {
        let len = self.label_stack.len();
        let idx = len
            .checked_sub(1 + depth as usize)
            .ok_or_else(|| LlvmError::Codegen(format!("label_depth {depth} out of range")))?;
        Ok(&self.label_stack[idx])
    }

    fn emit_br(&mut self, label_depth: u32) -> Result<(), LlvmError> {
        let target = self.label_target(label_depth)?;
        let bb = match target.kind {
            LabelKind::Block => target.tail_bb,
            LabelKind::Loop => target.header_bb,
        };
        self.builder
            .build_unconditional_branch(bb)
            .map_err(|e| LlvmError::Codegen(format!("Br: {e}")))?;
        // After a `Br`, the rest of the surrounding body is
        // unreachable in wasm semantics. LLVM does not allow
        // emitting more instructions into a terminated block — we
        // open a fresh `unreachable_after_br` block so the
        // emitter's invariants stay satisfied. The block stays
        // dead; LLVM's verifier and -O2 prune it.
        let dead_bb = self
            .ctx
            .append_basic_block(self.func, "unreachable_after_br");
        self.builder.position_at_end(dead_bb);
        // Seal it with an `unreachable` so the verifier accepts the
        // dead block before -O2 cleans it up.
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("dead-block unreachable: {e}")))?;
        // Reposition to a fresh successor so subsequent ops have an
        // open block to emit into. The successor will itself become
        // dead, but the verifier is happy with the chain.
        let cont_bb = self.ctx.append_basic_block(self.func, "after_br_cont");
        self.builder.position_at_end(cont_bb);
        Ok(())
    }

    fn emit_br_if(&mut self, ip_hint: &str, label_depth: u32) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        // Narrow the i32 / i64 condition to i1.
        let zero = cond.get_type().const_zero();
        let name = self.next_name("br_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, zero, &name)
            .map_err(|e| LlvmError::Codegen(format!("BrIf cmp: {e}")))?;
        let target = self.label_target(label_depth)?;
        let take_bb = match target.kind {
            LabelKind::Block => target.tail_bb,
            LabelKind::Loop => target.header_bb,
        };
        // Fall-through path stays in the surrounding body.
        let fallthru_bb = self.ctx.append_basic_block(self.func, "br_if_fallthru");
        self.builder
            .build_conditional_branch(cond_i1, take_bb, fallthru_bb)
            .map_err(|e| LlvmError::Codegen(format!("BrIf: {e}")))?;
        self.builder.position_at_end(fallthru_bb);
        Ok(())
    }

    fn emit_if(
        &mut self,
        ip_hint: &str,
        result_ty: IrType,
        then_body: &[TaggedOp],
        else_body: &[TaggedOp],
    ) -> Result<(), LlvmError> {
        let cond = self.pop_int(ip_hint)?;
        let name = self.next_name("if_cond");
        let cond_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, cond, cond.get_type().const_zero(), &name)
            .map_err(|e| LlvmError::Codegen(format!("If cmp: {e}")))?;
        let then_bb = self.ctx.append_basic_block(self.func, "if_then");
        let else_bb = self.ctx.append_basic_block(self.func, "if_else");
        let merge_bb = self.ctx.append_basic_block(self.func, "if_merge");
        self.builder
            .build_conditional_branch(cond_i1, then_bb, else_bb)
            .map_err(|e| LlvmError::Codegen(format!("If branch: {e}")))?;

        // Devirtualisation (W18) correctness: the `KnownClosure`
        // let-slot tracker is path-insensitive (it mutates a flat map as
        // the emitter walks ops). Across an `If`, the two arms may bind
        // the *same* let-slot to *different* known closures; a flat
        // last-write would let a post-merge `LetGet` devirtualise to the
        // wrong target on the path that didn't run — a miscompile. Take
        // the dataflow *meet*: snapshot the map, emit each arm against
        // its own copy, then keep only entries both arms agree on (and
        // entries neither touched). Disagreements are dropped, so the
        // post-merge `LetGet` falls back to the runtime switch (always
        // correct). Straight-line shapes like W18 are unaffected.
        let known_closure_snapshot = self.known_closure_let_slots.clone();

        // Then arm.
        self.builder.position_at_end(then_bb);
        for (ip, tagged) in then_body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        let then_result = self.pop(ip_hint).ok();
        let then_known_closures = std::mem::replace(
            &mut self.known_closure_let_slots,
            known_closure_snapshot.clone(),
        );
        let then_end_bb = self.builder.get_insert_block().unwrap();
        let then_terminated = then_end_bb.get_terminator().is_some();
        if !then_terminated {
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| LlvmError::Codegen(format!("If then->merge: {e}")))?;
        }

        // Else arm (starts from the pre-If snapshot, restored above).
        self.builder.position_at_end(else_bb);
        for (ip, tagged) in else_body.iter().enumerate() {
            self.lower_op(ip, tagged)?;
        }
        let else_result = self.pop(ip_hint).ok();
        let else_known_closures =
            std::mem::replace(&mut self.known_closure_let_slots, known_closure_snapshot);
        let else_end_bb = self.builder.get_insert_block().unwrap();
        let else_terminated = else_end_bb.get_terminator().is_some();
        if !else_terminated {
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| LlvmError::Codegen(format!("If else->merge: {e}")))?;
        }
        // Meet: keep a `KnownClosure` slot only when both arms reached
        // the merge with the SAME known target. A slot only one arm
        // touched, or that the arms disagree on, is dropped. An arm that
        // terminated (e.g. `Return`) cannot reach the merge, so the
        // surviving arm's view governs the slots it owns.
        self.known_closure_let_slots = match (then_terminated, else_terminated) {
            (true, true) => std::collections::HashMap::new(),
            (true, false) => else_known_closures,
            (false, true) => then_known_closures,
            (false, false) => then_known_closures
                .iter()
                .filter_map(|(&k, &v)| (else_known_closures.get(&k) == Some(&v)).then_some((k, v)))
                .collect(),
        };

        // Merge phi if both arms terminated normally.
        self.builder.position_at_end(merge_bb);
        match (then_result, else_result) {
            (Some(t), Some(e)) => {
                let phi_ty: inkwell::types::BasicTypeEnum<'ctx> = match result_ty {
                    // AOT-1: F64 rides as i64 bits, so both arms feed an
                    // i64-typed phi (the bit pattern, never a `double`).
                    IrType::I64 | IrType::F64 => self.ctx.i64_type().into(),
                    IrType::I32 | IrType::Bool | IrType::Null => self.ctx.i32_type().into(),
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "If result_ty {other:?} unsupported"
                        )));
                    }
                };
                let phi = self
                    .builder
                    .build_phi(phi_ty, "if_phi")
                    .map_err(|e| LlvmError::Codegen(format!("If phi: {e}")))?;
                let then_val: BasicValueEnum<'ctx> = t.val.into();
                let else_val: BasicValueEnum<'ctx> = e.val.into();
                if !then_terminated {
                    phi.add_incoming(&[(&then_val, then_end_bb)]);
                }
                if !else_terminated {
                    phi.add_incoming(&[(&else_val, else_end_bb)]);
                }
                let v = phi.as_basic_value().into_int_value();
                self.push(v, result_ty);
            }
            _ => {
                // One arm didn't push (e.g. ended with Return).
                // Phase B's W1/W2 path doesn't exercise this — surface
                // an error so a future shape doesn't silently miscompile.
                if !then_terminated || !else_terminated {
                    return Err(LlvmError::Codegen(
                        "If arms produced no value but did not terminate".into(),
                    ));
                }
                // Both arms terminated (e.g. both Return). Surface
                // `merge_bb` as unreachable.
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("If merge unreachable: {e}")))?;
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
}

impl BinOp {
    fn name(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "sdiv",
            BinOp::Mod => "srem",
            BinOp::BitAnd => "and",
        }
    }
}

/// Inline lookup table used by `emit_load_field`. Picks the LLVM
/// integer type + the IR tag we push back onto the operand stack
/// for a Phase-B-supported scalar field type.
impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    fn field_load_kind(
        &self,
        ty: IrType,
    ) -> Result<(inkwell::types::BasicTypeEnum<'ctx>, IrType), LlvmError> {
        let pair: (inkwell::types::BasicTypeEnum<'ctx>, IrType) = match ty {
            IrType::I64 => (self.ctx.i64_type().into(), IrType::I64),
            IrType::I32 => (self.ctx.i32_type().into(), IrType::I32),
            IrType::F64 => (self.ctx.f64_type().into(), IrType::F64),
            IrType::Bool => (self.ctx.i8_type().into(), IrType::Bool),
            IrType::Null => (self.ctx.i8_type().into(), IrType::Null),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "LoadField: Phase B envelope rejects {other:?}"
                )));
            }
        };
        Ok(pair)
    }

    /// Phase 2 surface-widening: lower `Op::ReadStringLen` — pop an
    /// arena-relative i32 record pointer (String or List* header),
    /// load the leading 4-byte length prefix, zext to i64, push.
    /// Mirrors `relon-codegen-cranelift::codegen::field::emit_read_string_len`.
    ///
    /// No bounds check today (Phase B/C/D LLVM emitter doesn't emit
    /// `cond_trap`; Phase 3 wires the trap-propagation work).
    fn emit_read_string_len(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let ptr_i32 = self.pop_int(ip_hint)?;
        let addr = self.arena_addr_i32(ptr_i32)?;
        let name = self.next_name("rs_len");
        let len_i32 = self
            .builder
            .build_load(self.ctx.i32_type(), addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("ReadStringLen load: {e}")))?
            .into_int_value();
        let name = self.next_name("rs_len64");
        let len_i64 = self
            .builder
            .build_int_z_extend(len_i32, self.ctx.i64_type(), &name)
            .map_err(|e| LlvmError::Codegen(format!("ReadStringLen zext: {e}")))?;
        self.push(len_i64, IrType::I64);
        Ok(())
    }

    /// Phase 2 surface-widening: lower `Op::LoadStringPtr` (and its
    /// List* siblings) — `#main`-side String / List parameter loads.
    ///
    /// The IR's lowering pass emits this op whenever a `#main(String s)`
    /// (or List-typed) parameter is referenced; the buffer-protocol
    /// trampoline laid the matching record pointer (a 4-byte
    /// buffer-relative offset) at `offset` bytes inside the input
    /// record. We materialise the offset on the operand stack as an
    /// `IrType::String` (or matching List type) so downstream ops
    /// (`ReadStringLen`, `Op::Call { contains }`, list-method
    /// dispatch) see the same shape they would inside a freshly-
    /// produced literal.
    ///
    /// `IR LocalGet(0)` reads the buffer-protocol entry's `in_ptr`
    /// param (slot 1 on LLVM under `param_base = 1`); the pointer-
    /// indirect slot lives at that address plus `offset`. The
    /// resulting load is a plain i32, so we don't go through
    /// `field_load_kind`'s zext / type-tagging logic.
    fn emit_load_pointer_indirect_param(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::Load*Ptr({ty:?}) outside buffer-protocol entry shape"
            ))
        })?;
        let in_ptr_i32 = self.lookup_param(0)?; // IR LocalGet(0) == in_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, in_ptr_i32, offset)?;
        let name = self.next_name("loadptr");
        let raw = self
            .builder
            .build_load(self.ctx.i32_type(), addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("Load*Ptr load: {e}")))?
            .into_int_value();
        self.push(raw, ty);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Phase E.1: raw-memory primitives, scratch allocator, StrConcatN.
// ---------------------------------------------------------------------------

/// Variants of the absolute-pointer load lowering paths.
#[derive(Clone, Copy)]
enum AbsLoad {
    I32,
    I64,
    I8U,
    F64,
}

/// Variants of the absolute-pointer store lowering paths.
#[derive(Clone, Copy)]
enum AbsStore {
    I32,
    I64,
    I8,
    F64,
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Compute `arena_base + off_i32` as an LLVM pointer. Mirrors
    /// `Codegen::arena_addr` on the cranelift side — used by every
    /// `*AtAbsolute` lowering path. No bounds check (Phase E.1 retains
    /// the same "trust the IR + LLVM trap on UB" stance as Phase B).
    fn arena_addr_i32(&mut self, off_i32: IntValue<'ctx>) -> Result<PointerValue<'ctx>, LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("absolute load/store outside buffer-protocol entry shape".into())
        })?;
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        let name = self.next_name("abs_off_zext");
        let off64 = self
            .builder
            .build_int_z_extend(off_i32, i64_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("abs offset zext: {e}")))?;
        let name = self.next_name("abs_addr");
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[off64], &name)
                .map_err(|e| LlvmError::Codegen(format!("abs GEP: {e}")))?
        };
        Ok(addr)
    }

    /// Compose `base + offset` (both i32) into the absolute pointer
    /// each `Load*AtAbsolute` / `Store*AtAbsolute` op reads from.
    fn compose_abs_addr(
        &mut self,
        base: IntValue<'ctx>,
        offset: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let off_const = self.ctx.i32_type().const_int(u64::from(offset), false);
        let name = self.next_name("abs_compose");
        let composed = self
            .builder
            .build_int_add(base, off_const, &name)
            .map_err(|e| LlvmError::Codegen(format!("abs compose add: {e}")))?;
        self.arena_addr_i32(composed)
    }

    fn emit_load_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        kind: AbsLoad,
    ) -> Result<(), LlvmError> {
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        match kind {
            AbsLoad::I32 => {
                let name = self.next_name("loadi32_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.i32_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI32AtAbsolute: {e}")))?
                    .into_int_value();
                self.push(v, IrType::I32);
            }
            AbsLoad::I64 => {
                let name = self.next_name("loadi64_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.i64_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI64AtAbsolute: {e}")))?
                    .into_int_value();
                self.push(v, IrType::I64);
            }
            AbsLoad::I8U => {
                let name = self.next_name("loadi8u_abs");
                let b = self
                    .builder
                    .build_load(self.ctx.i8_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI8UAtAbsolute: {e}")))?
                    .into_int_value();
                let name = self.next_name("loadi8u_zext");
                let v = self
                    .builder
                    .build_int_z_extend(b, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI8UAtAbsolute zext: {e}")))?;
                self.push(v, IrType::I32);
            }
            AbsLoad::F64 => {
                // Float ops are outside the present W3/W4 envelope; we
                // still accept LoadF64AtAbsolute to keep the dispatcher
                // exhaustive. The stack carries the bit-cast i64.
                let name = self.next_name("loadf64_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.f64_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadF64AtAbsolute: {e}")))?;
                // Bit-cast to i64 to feed the int-typed virtual stack.
                let i64_t = self.ctx.i64_type();
                let name = self.next_name("loadf64_bitcast");
                let bits = self
                    .builder
                    .build_bit_cast(v, i64_t, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadF64 bitcast: {e}")))?
                    .into_int_value();
                self.push(bits, IrType::F64);
            }
        }
        Ok(())
    }

    fn emit_store_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        kind: AbsStore,
    ) -> Result<(), LlvmError> {
        // Stack: `[base, value]` — top is the value, below it is the
        // base. Mirrors cranelift's pop order.
        let value = self.pop_int(ip_hint)?;
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        match kind {
            AbsStore::I32 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI32AtAbsolute: {e}")))?;
            }
            AbsStore::I64 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI64AtAbsolute: {e}")))?;
            }
            AbsStore::I8 => {
                // Narrow the i32 value to i8 before the store.
                let name = self.next_name("storei8_trunc");
                let narrowed = self
                    .builder
                    .build_int_truncate(value, self.ctx.i8_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI8AtAbsolute trunc: {e}")))?;
                self.builder
                    .build_store(addr, narrowed)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI8AtAbsolute: {e}")))?;
            }
            AbsStore::F64 => {
                // The IR's virtual stack carries f64 as bit-cast i64;
                // bit-cast back before the store so the destination
                // bytes match the wasm f64 wire layout.
                let name = self.next_name("storef64_bitcast");
                let f = self
                    .builder
                    .build_bit_cast(value, self.ctx.f64_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreF64 bitcast: {e}")))?;
                self.builder
                    .build_store(addr, f)
                    .map_err(|e| LlvmError::Codegen(format!("StoreF64AtAbsolute: {e}")))?;
            }
        }
        Ok(())
    }

    /// Lower `Op::MemcpyAtAbsolute`. Stack: `[dst, src, len]`. Calls
    /// LLVM's `llvm.memcpy.p0.p0.i64` intrinsic with both pointers
    /// resolved through `arena_base`.
    fn emit_memcpy_at_absolute(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let len = self.pop_int(ip_hint)?;
        let src = self.pop_int(ip_hint)?;
        let dst = self.pop_int(ip_hint)?;
        let dst_ptr = self.arena_addr_i32(dst)?;
        let src_ptr = self.arena_addr_i32(src)?;
        // `inkwell`'s `build_memcpy` requires the length to be the
        // pointer-width int. Widen our i32 length to i64 (zero-extend).
        let i64_t = self.ctx.i64_type();
        let len64 = self
            .builder
            .build_int_z_extend(len, i64_t, "memcpy_len_zext")
            .map_err(|e| LlvmError::Codegen(format!("memcpy len zext: {e}")))?;
        // Pick a 1-byte alignment hint — the inner records aren't
        // guaranteed > 1-byte aligned (string headers land on 4-byte
        // boundaries but their payload follows immediately). The LLVM
        // optimiser will refine when it can prove a tighter bound.
        self.builder
            .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
            .map_err(|e| LlvmError::Codegen(format!("MemcpyAtAbsolute build: {e}")))?;
        Ok(())
    }

    /// Bump-allocate `size_v` (i32) bytes inside the arena's scratch
    /// region. Pushes the pre-bump cursor as an arena-relative i32
    /// offset onto the virtual stack — same shape as cranelift's
    /// `emit_alloc_scratch`.
    fn emit_alloc_scratch_common(&mut self, size_v: IntValue<'ctx>) -> Result<(), LlvmError> {
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "AllocScratch outside buffer-protocol entry shape (no state ptr)".into(),
            )
        })?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();

        // GEP-then-load helpers. We hand-roll the i8-offset GEPs
        // because the inkwell wrappers expect a struct field accessor.
        let cursor_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_CURSOR), false)],
                    "scratch_cursor_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_cursor GEP: {e}")))?
        };
        let cur = self
            .builder
            .build_load(i32_t, cursor_gep, "scratch_cursor")
            .map_err(|e| LlvmError::Codegen(format!("scratch_cursor load: {e}")))?
            .into_int_value();
        let base_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_BASE), false)],
                    "scratch_base_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_base GEP: {e}")))?
        };
        let scratch_base = self
            .builder
            .build_load(i32_t, base_gep, "scratch_base")
            .map_err(|e| LlvmError::Codegen(format!("scratch_base load: {e}")))?
            .into_int_value();

        // Returned arena-relative offset = scratch_base + cur.
        let off = self
            .builder
            .build_int_add(scratch_base, cur, "scratch_off")
            .map_err(|e| LlvmError::Codegen(format!("scratch off add: {e}")))?;
        // New cursor = cur + size.
        let new_cur = self
            .builder
            .build_int_add(cur, size_v, "scratch_new_cur")
            .map_err(|e| LlvmError::Codegen(format!("scratch cur bump: {e}")))?;
        self.builder
            .build_store(cursor_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("scratch cursor store: {e}")))?;
        self.push(off, IrType::I32);
        Ok(())
    }

    fn emit_alloc_scratch_static(&mut self, size_bytes: u32) -> Result<(), LlvmError> {
        let size_v = self.ctx.i32_type().const_int(u64::from(size_bytes), false);
        self.emit_alloc_scratch_common(size_v)
    }

    fn emit_alloc_scratch_dyn(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let size = self.pop_int(ip_hint)?;
        self.emit_alloc_scratch_common(size)
    }

    /// Resolve / create the i32 alloca backing an
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord` record-local
    /// index. Each variable holds an out_ptr-relative i32 offset.
    /// Mirrors cranelift's `get_or_create_record_local`.
    fn get_or_create_record_local(&mut self, idx: u32) -> Result<PointerValue<'ctx>, LlvmError> {
        if let Some(p) = self.record_locals.get(&idx).copied() {
            return Ok(p);
        }
        let i32_t = self.ctx.i32_type();
        let name = self.next_name("record_local");
        let slot = self
            .builder
            .build_alloca(i32_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("record_local alloca: {e}")))?;
        self.record_locals.insert(idx, slot);
        Ok(slot)
    }

    /// Lower `Op::AllocRootRecord { record_local_idx }`. The root
    /// record sits at `out_ptr + 0`; bind the record-local to constant
    /// `i32 0`. Subsequent `Op::StoreFieldAtRecord` ops uniformly
    /// compute `out_ptr + record_local + offset` from this slot.
    /// Mirrors cranelift's `emit_alloc_root_record`.
    fn emit_alloc_root_record(&mut self, idx: u32) -> Result<(), LlvmError> {
        // Phase D.2: fast-path entry has no arena to write into — the
        // matching `StoreFieldAtRecord` is rewritten to a store
        // against the `fast_ret_slot` alloca, which doesn't need a
        // record-local offset. Skip the alloca entirely so post-O3
        // IR stays free of the dead bookkeeping store.
        if self.fast_path.is_some() {
            let _ = idx;
            return Ok(());
        }
        let slot = self.get_or_create_record_local(idx)?;
        let zero = self.ctx.i32_type().const_zero();
        self.builder
            .build_store(slot, zero)
            .map_err(|e| LlvmError::Codegen(format!("AllocRootRecord store: {e}")))?;
        Ok(())
    }

    /// Lower `Op::StoreFieldAtRecord { record_local_idx, offset, ty }`.
    /// Pops the top of the operand stack and writes it into
    /// `out_ptr + record_local + offset`. Mirrors cranelift's
    /// `emit_store_field_at_record` but without the explicit bounds
    /// check (LLVM AOT relies on the host's arena sizing).
    fn emit_store_field_at_record(
        &mut self,
        ip_hint: &str,
        idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        // Phase D.2: fast-path entry rewrites the single-Int-field
        // record store into the `fast_ret_slot` store. Mirrors the
        // `Op::StoreField` rewrite — the profile gate guarantees the
        // return record carries exactly one Int field, so the matching
        // `StoreFieldAtRecord` at `profile.ret_offset` is the
        // function's actual return value. Any other shape (multi-
        // field record, branded sub-records) escapes the envelope
        // and surfaces as an emitter error.
        if let Some(fast) = self.fast_path.clone() {
            let _ = idx;
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreFieldAtRecord: only I64 returns supported, got {ty:?}"
                )));
            }
            if offset != fast.profile.ret_offset {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreFieldAtRecord: offset {offset} != profile.ret_offset {}",
                    fast.profile.ret_offset
                )));
            }
            let v = self.pop_int(ip_hint)?;
            self.builder.build_store(fast.ret_slot, v).map_err(|e| {
                LlvmError::Codegen(format!("fast StoreFieldAtRecord ret_slot: {e}"))
            })?;
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreFieldAtRecord outside buffer-protocol entry".into())
        })?;
        let value = self.pop_int(ip_hint)?;
        let slot = self.record_locals.get(&idx).copied().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "StoreFieldAtRecord({idx}) before matching AllocRootRecord"
            ))
        })?;
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        // Read the record-local offset, add `offset`, add `out_ptr`,
        // then z-extend the sum into the i64 arena GEP index.
        let record_base = self
            .builder
            .build_load(i32_t, slot, "record_base")
            .map_err(|e| LlvmError::Codegen(format!("record_base load: {e}")))?
            .into_int_value();
        let off_const = i32_t.const_int(u64::from(offset), false);
        let slot_off = self
            .builder
            .build_int_add(record_base, off_const, "record_slot_off")
            .map_err(|e| LlvmError::Codegen(format!("record_slot_off: {e}")))?;
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr under buffer protocol
        let total_off = self
            .builder
            .build_int_add(out_ptr_i32, slot_off, "record_total_off")
            .map_err(|e| LlvmError::Codegen(format!("record_total_off: {e}")))?;
        let total_off64 = self
            .builder
            .build_int_z_extend(total_off, i64_t, "record_total_off_zext")
            .map_err(|e| LlvmError::Codegen(format!("record_total_off zext: {e}")))?;
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[total_off64], "record_dst")
                .map_err(|e| LlvmError::Codegen(format!("record_dst GEP: {e}")))?
        };
        // Emit the typed store. For `Bool` / `Null`, narrow the i32
        // stack slot to i8 before writing — matches the on-wire
        // record layout. For pointer-indirect types (`String`,
        // `List*`) the slot stores the i32 buffer-relative offset
        // verbatim.
        match ty {
            IrType::I64 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord I64: {e}")))?;
            }
            IrType::F64 => {
                // Stack carries f64 as bit-cast i64; restore the f64
                // payload for the store so the destination bytes
                // match the IEEE-754 wire layout.
                let f = self
                    .builder
                    .build_bit_cast(value, self.ctx.f64_type(), "record_f64_bitcast")
                    .map_err(|e| LlvmError::Codegen(format!("F64 bitcast: {e}")))?;
                self.builder
                    .build_store(addr, f)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord F64: {e}")))?;
            }
            IrType::I32
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::Closure => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord I32: {e}")))?;
            }
            IrType::Bool | IrType::Null => {
                let v8 = self
                    .builder
                    .build_int_truncate(value, i8_t, "record_bool_trunc")
                    .map_err(|e| {
                        LlvmError::Codegen(format!("StoreFieldAtRecord Bool trunc: {e}"))
                    })?;
                self.builder
                    .build_store(addr, v8)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord Bool: {e}")))?;
            }
        }
        Ok(())
    }

    /// Phase F.W7: lower `Op::MakeClosure { fn_table_idx, captures,
    /// captures_size }`. Closure handle layout (8 bytes total):
    ///   `[fn_table_idx: u32 LE][captures_ptr: u32 LE]`
    ///
    /// Steps:
    ///   1. Alloc 8 bytes for the handle (arena-relative ptr ->
    ///      `handle_ptr`).
    ///   2. If `captures_size > 0`: alloc `captures_size` bytes for
    ///      the captures struct (-> `captures_ptr`). For each
    ///      capture, write the matching let-local value into the
    ///      struct at the declared offset. **Self-recursion** is
    ///      detected by a missing let_slot at MakeClosure time — the
    ///      lowering pass places the self-binding's `LetSet` *after*
    ///      MakeClosure, so the about-to-be-stored handle is the only
    ///      value the captured slot could hold. We seed it with
    ///      `handle_ptr` (the value the upcoming `LetSet` will stash)
    ///      so the recursive call site reads back a live handle
    ///      instead of zero (which would crash `CallClosure`'s
    ///      indirect dispatch).
    ///   3. Store `fn_table_idx` at `handle_ptr + 0`.
    ///   4. Store `captures_ptr` (or 0) at `handle_ptr + 4`.
    ///   5. Push `handle_ptr` as the i32 Closure handle.
    fn emit_make_closure(
        &mut self,
        ip_hint: &str,
        fn_table_idx: u32,
        captures: &[relon_ir::ir::ClosureCapture],
        captures_size: u32,
    ) -> Result<(), LlvmError> {
        let _ = ip_hint;
        // Validate fn_table_idx against the closure table the emit
        // pass seeded. The IR lowering numbers slots in source order;
        // a slot >= table length means the lowering pass and emit
        // pass disagree on closure count.
        if (fn_table_idx as usize) >= self.closure_fn_table.len() {
            return Err(LlvmError::Codegen(format!(
                "MakeClosure fn_table_idx={fn_table_idx} out of range (closure_fn_table.len()={})",
                self.closure_fn_table.len()
            )));
        }
        // Phase D.2: fast-path entry has no arena/state to bump the
        // 8-byte handle / captures-struct into. Virtualise the closure
        // — push a placeholder i32 tagged with `FastPathClosure` so the
        // downstream `LetSet/LetGet` chain keeps the `fn_table_idx`
        // available, and the matching `CallClosure` rewrites into a
        // direct call against `closure_fn_table[fn_table_idx]`. Sound
        // for the W7 anon-Dict shape because the lambda's post-O3
        // body drops state / captures_ptr (the inner self-recursion
        // fast path already side-stepped them); passing zero through
        // the direct-call ABI lets LLVM strip the dead args entirely.
        //
        // Captures are skipped — the only legal capture in this
        // shape is the self-handle (an `Op::LetSet { ty: Closure }`
        // immediately follows the `MakeClosure`) which the fast-path
        // closure tracker re-derives from `fn_table_idx`. Any other
        // capture surfaces as an emitter error so future widenings
        // (W3 fold, W11 multi-closure) explicitly opt in.
        if self.fast_path.is_some() {
            for cap in captures {
                if !matches!(cap.ty, IrType::Closure) {
                    return Err(LlvmError::Codegen(format!(
                        "fast-path MakeClosure: non-Closure capture (let_idx={}, ty={:?}) \
                         outside the W7 envelope",
                        cap.let_idx, cap.ty
                    )));
                }
            }
            let _ = captures_size;
            let placeholder = self.ctx.i32_type().const_zero();
            self.push_with_prov(
                placeholder,
                IrType::Closure,
                Provenance::FastPathClosure { fn_table_idx },
            );
            return Ok(());
        }
        // Step 1: alloc 8 bytes for the handle.
        let i32_t = self.ctx.i32_type();
        let eight = i32_t.const_int(8, false);
        self.emit_alloc_scratch_common(eight)?;
        let handle_ptr = self.pop_int("MakeClosure handle alloc")?;

        // Step 2: alloc + populate the captures struct.
        let captures_ptr = if captures_size > 0 {
            let cs = i32_t.const_int(u64::from(captures_size), false);
            self.emit_alloc_scratch_common(cs)?;
            self.pop_int("MakeClosure captures alloc")?
        } else {
            i32_t.const_zero()
        };

        // Step 3: store fn_table_idx at handle_ptr + 0.
        let fn_idx_v = i32_t.const_int(u64::from(fn_table_idx), false);
        let handle_addr = self.arena_addr_i32(handle_ptr)?;
        self.builder
            .build_store(handle_addr, fn_idx_v)
            .map_err(|e| LlvmError::Codegen(format!("MakeClosure fn_idx store: {e}")))?;

        // Step 4: store captures_ptr at handle_ptr + 4.
        let four = self.ctx.i32_type().const_int(4, false);
        let handle_plus_4 = self
            .builder
            .build_int_add(handle_ptr, four, "handle_plus_4")
            .map_err(|e| LlvmError::Codegen(format!("MakeClosure handle+4: {e}")))?;
        let captures_slot_addr = self.arena_addr_i32(handle_plus_4)?;
        self.builder
            .build_store(captures_slot_addr, captures_ptr)
            .map_err(|e| LlvmError::Codegen(format!("MakeClosure captures store: {e}")))?;

        // Step 5: write each capture into the captures struct.
        if captures_size > 0 {
            for cap in captures {
                // Determine the value to stash. If a let-slot exists
                // for `cap.let_idx`, read it. Otherwise treat it as a
                // self-recursive capture and use the handle_ptr we
                // just allocated (matches what the immediately-
                // following `LetSet { idx: cap.let_idx, ty: Closure }`
                // will store).
                let mapped_idx = self.remap_let_idx(cap.let_idx);
                let cap_offset = self.ctx.i32_type().const_int(u64::from(cap.offset), false);
                let cap_addr_i32 = self
                    .builder
                    .build_int_add(captures_ptr, cap_offset, "cap_off")
                    .map_err(|e| LlvmError::Codegen(format!("MakeClosure cap off: {e}")))?;
                let cap_addr = self.arena_addr_i32(cap_addr_i32)?;
                let value: BasicValueEnum<'ctx> = if let Some((slot, slot_ty)) =
                    self.let_slots.get(&mapped_idx).copied()
                {
                    let load_name = self.next_name("cap_load");
                    let raw = self
                        .builder
                        .build_load(self.ir_ty_to_llvm_int(slot_ty)?, slot, &load_name)
                        .map_err(|e| LlvmError::Codegen(format!("MakeClosure cap let load: {e}")))?
                        .into_int_value();
                    // Coerce to the capture's declared IR type
                    // width — the let-slot may have stored a
                    // wider value (e.g. i32 Closure stashed as
                    // i32 already matches; widen-and-truncate is
                    // a no-op).
                    match cap.ty {
                        IrType::I64 => {
                            if raw.get_type().get_bit_width() < 64 {
                                self.builder
                                    .build_int_z_extend(raw, self.ctx.i64_type(), "cap_zext")
                                    .map_err(|e| {
                                        LlvmError::Codegen(format!("MakeClosure cap zext: {e}"))
                                    })?
                                    .into()
                            } else {
                                raw.into()
                            }
                        }
                        IrType::F64 => {
                            // Stack carries f64 bit-cast to i64;
                            // store the bit pattern verbatim
                            // (the load on the read side bit-
                            // casts back).
                            raw.into()
                        }
                        _ => {
                            // Narrow to i32 if the let-slot
                            // carries i64; cap.ty is one of the
                            // 4-byte-wide variants.
                            if raw.get_type().get_bit_width() > 32 {
                                self.builder
                                    .build_int_truncate(raw, self.ctx.i32_type(), "cap_trunc")
                                    .map_err(|e| {
                                        LlvmError::Codegen(format!("MakeClosure cap trunc: {e}"))
                                    })?
                                    .into()
                            } else {
                                raw.into()
                            }
                        }
                    }
                } else {
                    // Self-recursive capture: the let-slot for
                    // `mapped_idx` isn't initialised yet because
                    // the lowering pass emits MakeClosure before
                    // the matching `LetSet`. The captured value
                    // is the closure handle itself — the same
                    // value the upcoming `LetSet` will store —
                    // so we stamp `handle_ptr` here. Only legal
                    // when the capture's IR type is `Closure`
                    // (anything else can't refer to a
                    // not-yet-bound let-local in source).
                    if cap.ty != IrType::Closure {
                        return Err(LlvmError::Codegen(format!(
                                "MakeClosure capture `let_idx={mapped_idx}` not yet bound but ty={:?} (expected Closure for self-recursion)",
                                cap.ty
                            )));
                    }
                    handle_ptr.into()
                };
                self.builder
                    .build_store(cap_addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("MakeClosure cap store: {e}")))?;
            }
        }

        // Step 6: push the handle_ptr, tagged with the compile-time
        // `fn_table_idx` we just stored at `handle_ptr + 0`. The handle
        // is a real, fully-populated arena record (captures_ptr live at
        // `+4`), so a later `CallClosure` that consumes *this exact
        // value* can skip the runtime `switch i32 %cc_fn_idx` — its
        // selector is provably this constant — and emit a direct call
        // while still loading the real captures_ptr. Devirtualisation
        // fires only when the value reaches the call site unmodified
        // (tracked through `LetSet`/`LetGet` + inline-frame param binds);
        // any reassignment drops the provenance and the slow-path switch
        // returns. See [`Provenance::KnownClosure`].
        self.push_with_prov(
            handle_ptr,
            IrType::Closure,
            Provenance::KnownClosure { fn_table_idx },
        );
        Ok(())
    }

    /// Phase F.W7: lower `Op::CallClosure { param_tys, ret_ty }`.
    /// Stack discipline: `[Closure, arg0, arg1, ...] -> [ret_ty]`.
    ///
    /// Pops user args (in reverse), pops the closure handle,
    /// materialises `fn_table_idx` + `captures_ptr` from the handle,
    /// looks up the matching `FunctionValue` through
    /// `closure_fn_table[fn_table_idx]` via a switch, and invokes
    /// the resolved function indirectly with
    /// `(state, captures_ptr, args...)`.
    fn emit_call_closure(
        &mut self,
        ip_hint: &str,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        if self.closure_fn_table.is_empty() {
            return Err(LlvmError::Codegen(
                "Op::CallClosure but closure_fn_table is empty — module declared no lambdas".into(),
            ));
        }
        // Phase D.2: fast-path entry routes `CallClosure` through a
        // direct call to `closure_fn_table[fn_table_idx]` when the
        // popped handle was produced by an in-body `MakeClosure`
        // (virtualised closure carrying `FastPathClosure` provenance).
        // The lambda's `(state, captures_ptr, args...)` signature is
        // satisfied with a null pointer + i32 zero — sound for the W7
        // anon-Dict shape because the lambda's post-O3 body has
        // already dropped both args (the inner self-recursion fast
        // path side-stepped them). LLVM strips the dead loads when
        // it inlines the call.
        if self.fast_path.is_some() {
            // Pop user args in reverse.
            let mut user_args: Vec<IntValue<'ctx>> = Vec::with_capacity(param_tys.len());
            for _ in 0..param_tys.len() {
                user_args.push(self.pop_int(ip_hint)?);
            }
            user_args.reverse();
            let handle_tv = self.pop(ip_hint)?;
            let fn_table_idx = match handle_tv.prov {
                Provenance::FastPathClosure { fn_table_idx } => fn_table_idx,
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "fast-path CallClosure: handle has provenance {other:?} (expected \
                         FastPathClosure — the call site reads a closure not constructed \
                         in this entry's body, outside the W7 envelope)"
                    )));
                }
            };
            let slot = fn_table_idx as usize;
            if slot >= self.closure_fn_table.len() {
                return Err(LlvmError::Codegen(format!(
                    "fast-path CallClosure: fn_table_idx={fn_table_idx} out of range \
                     (closure_fn_table.len()={})",
                    self.closure_fn_table.len()
                )));
            }
            let callee = self.closure_fn_table[slot];
            let null_state = self.ctx.ptr_type(AddressSpace::default()).const_null();
            let null_captures = self.ctx.i32_type().const_zero();
            return self.emit_call_closure_direct(
                callee,
                null_state,
                null_captures,
                user_args,
                param_tys,
                ret_ty,
            );
        }
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen("CallClosure outside buffer-protocol entry (no state)".into())
        })?;
        // Pop user args in reverse.
        let mut user_args: Vec<IntValue<'ctx>> = Vec::with_capacity(param_tys.len());
        for _ in 0..param_tys.len() {
            user_args.push(self.pop_int(ip_hint)?);
        }
        user_args.reverse();

        // Pop closure handle (i32 arena-relative offset). Capture
        // provenance up-front so the self-recursion fast path can
        // route around the handle deref / switch entirely.
        let handle_tv = self.pop(ip_hint)?;
        let handle_ptr = handle_tv.val;
        let handle_prov = handle_tv.prov;

        // Phase F.W7 self-recursion fast path: when the handle came
        // from the lambda's own self-capture chain we know
        //  * `handle.fn_table_idx == self_fn_table_idx` (stamped by
        //    the outer `MakeClosure`); and
        //  * `handle.captures_ptr == captures_ptr_arg` (the lambda's
        //    LLVM param 1 — same value `MakeClosure` stashed into the
        //    handle's `+4` slot because the captured pointer is the
        //    captures struct the host built for this very lambda).
        // Skip the handle deref + switch dispatch and emit a direct
        // call to the matching `FunctionValue`. Cuts ~3 loads + a
        // conditional branch off every recursion, closing the gap
        // versus the equivalent Rust direct-recursive call on W7
        // (recursive `fib(k - 1) + fib(k - 2)`).
        if let (
            Provenance::OwnCaptureHandle {
                self_fn_table_idx, ..
            },
            Some(captures_ptr_arg),
        ) = (handle_prov, self.captures_ptr_param)
        {
            let slot = self_fn_table_idx as usize;
            if slot < self.closure_fn_table.len() {
                return self.emit_call_closure_direct(
                    self.closure_fn_table[slot],
                    state_ptr,
                    captures_ptr_arg,
                    user_args,
                    param_tys,
                    ret_ty,
                );
            }
        }

        // Devirtualisation (W18): the handle came from a literal
        // `MakeClosure` whose `fn_table_idx` is a compile-time constant
        // and the value reached this call site unmodified (tracked
        // through the `KnownClosure` provenance across `LetSet`/`LetGet`
        // and inline-frame argument binds). The runtime
        // `switch i32 %cc_fn_idx` would therefore *always* select
        // `closure_fn_table[fn_table_idx]`, so emit a direct call to it
        // — LLVM then inlines the callee, folding the per-element
        // dispatch out of the hot loop. We still load the *real*
        // captures_ptr from `handle + 4` (this closure may capture
        // free variables — e.g. the W18 predicate captures `is_prime`),
        // so capture semantics are byte-identical to the switch path;
        // only the dead `fn_idx` load + switch are removed.
        //
        // Correctness guard: devirtualise ONLY when the resolved
        // callee's signature (arity + return width) matches this call
        // site, exactly as the switch's per-case `signature_compatible`
        // check requires. A module may host several lambdas; if the
        // statically-resolved target somehow disagrees with the call
        // shape we keep the switch (its matching case fires at runtime),
        // never emitting an ill-typed direct call.
        if let Provenance::KnownClosure { fn_table_idx } = handle_prov {
            let slot = fn_table_idx as usize;
            if slot < self.closure_fn_table.len() {
                let callee = self.closure_fn_table[slot];
                let want_arity = 2 + user_args.len();
                let want_ret_llvm = match ret_ty {
                    IrType::Null => None,
                    other => Some(self.ir_ty_to_llvm_int(other)?.get_bit_width()),
                };
                let have_ret_llvm = callee.get_type().get_return_type().and_then(|t| match t {
                    inkwell::types::BasicTypeEnum::IntType(it) => Some(it.get_bit_width()),
                    _ => None,
                });
                let signature_compatible =
                    callee.count_params() as usize == want_arity && have_ret_llvm == want_ret_llvm;
                if signature_compatible {
                    // Load the real captures_ptr from `handle + 4`; the
                    // closure's captured environment must be passed
                    // verbatim (unchanged from the slow path).
                    let i32_t = self.ctx.i32_type();
                    let four = i32_t.const_int(4, false);
                    let handle_plus_4 = self
                        .builder
                        .build_int_add(handle_ptr, four, "ccd_handle_plus_4")
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(known) handle+4: {e}"))
                        })?;
                    let cap_ptr_addr = self.arena_addr_i32(handle_plus_4)?;
                    let captures_ptr_name = self.next_name("ccd_captures_ptr");
                    let captures_ptr = self
                        .builder
                        .build_load(i32_t, cap_ptr_addr, &captures_ptr_name)
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(known) captures load: {e}"))
                        })?
                        .into_int_value();
                    return self.emit_call_closure_direct(
                        callee,
                        state_ptr,
                        captures_ptr,
                        user_args,
                        param_tys,
                        ret_ty,
                    );
                }
            }
        }

        // Load fn_table_idx (handle+0) and captures_ptr (handle+4).
        let handle_addr = self.arena_addr_i32(handle_ptr)?;
        let i32_t = self.ctx.i32_type();
        let fn_idx_name = self.next_name("cc_fn_idx");
        let fn_idx = self
            .builder
            .build_load(i32_t, handle_addr, &fn_idx_name)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure fn_idx load: {e}")))?
            .into_int_value();
        let four = i32_t.const_int(4, false);
        let handle_plus_4 = self
            .builder
            .build_int_add(handle_ptr, four, "cc_handle_plus_4")
            .map_err(|e| LlvmError::Codegen(format!("CallClosure handle+4: {e}")))?;
        let cap_ptr_addr = self.arena_addr_i32(handle_plus_4)?;
        let captures_ptr_name = self.next_name("cc_captures_ptr");
        let captures_ptr = self
            .builder
            .build_load(i32_t, cap_ptr_addr, &captures_ptr_name)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure captures load: {e}")))?
            .into_int_value();

        // NOTE: per-arg width coercion is deferred into each switch
        // case below. A module may host several same-arity lambdas with
        // *different* param widths (AOT-4 W16 binds a `List<Int>`-taking
        // recursive `sum_qs` (i32 handle) alongside a 1-arg `(x: Int)`
        // filter predicate (i64) — both arity 1). Coercing once here to
        // this call site's `param_tys` and then reusing the coerced
        // values across every switch case would emit a wrong-width arg
        // into the sibling lambda's case (which is statically present
        // but dynamically dead) and the LLVM verifier rejects the whole
        // module. Coercing per-case against the *callee's* declared LLVM
        // param type keeps each case well-typed; the runtime
        // `fn_table_idx` only ever selects the case whose lambda matches
        // the handle, so the sibling cases are never executed.

        // Dispatch through a switch over fn_table_idx → one direct
        // call per lambda. This avoids needing a runtime function-
        // pointer table at module scope (LLVM 18 + opaque pointers
        // makes that doable but adds the burden of seeding the
        // global at JIT-resolve time). The switch IR is tiny and
        // LLVM's selectoptimize pass collapses it to a jump table /
        // computed call when profitable.
        let cur_bb = self
            .builder
            .get_insert_block()
            .ok_or_else(|| LlvmError::Codegen("CallClosure: builder has no insert block".into()))?;
        let post_bb = self.ctx.append_basic_block(self.func, "cc_post");
        // Pre-allocate the ret slot in the entry block so mem2reg can
        // promote it across the switch joins.
        let ret_slot = if !matches!(ret_ty, IrType::Null) {
            let ret_llvm_ty = self.ir_ty_to_llvm_int(ret_ty)?;
            let cur = self.builder.get_insert_block();
            // Position at entry block start to place the alloca
            // there; restore afterwards.
            let entry_first = self.func.get_first_basic_block().ok_or_else(|| {
                LlvmError::Codegen("CallClosure: function missing entry block".into())
            })?;
            // Insert before the first non-alloca instr — close enough
            // for mem2reg.
            if let Some(first) = entry_first.get_first_instruction() {
                self.builder.position_before(&first);
            } else {
                self.builder.position_at_end(entry_first);
            }
            let slot = self
                .builder
                .build_alloca(ret_llvm_ty, "cc_ret_slot")
                .map_err(|e| LlvmError::Codegen(format!("CallClosure ret_slot alloca: {e}")))?;
            // Restore builder position.
            if let Some(bb) = cur {
                self.builder.position_at_end(bb);
            }
            Some(slot)
        } else {
            None
        };

        // Build cases: one BB per lambda.
        let mut case_bbs: Vec<inkwell::basic_block::BasicBlock<'ctx>> =
            Vec::with_capacity(self.closure_fn_table.len());
        for slot in 0..self.closure_fn_table.len() {
            let bb = self
                .ctx
                .append_basic_block(self.func, &format!("cc_case_{slot}"));
            case_bbs.push(bb);
        }
        // Default trap block — execution reaches it only if the
        // handle's fn_table_idx is out of range, which would mean
        // memory corruption.
        let default_bb = self.ctx.append_basic_block(self.func, "cc_default_trap");

        // Position at the switch's current block and emit it.
        //
        // The switch's jump-table lowering is JIT-safe only because the
        // MCJIT memory manager allocates every code / data section
        // (including the `.rodata` jump table the `switch` lowers to) in
        // the low 2 GiB (`MAP_32BIT`) — the Small code model addresses
        // the table through a 32-bit *absolute* reference. See
        // `mcjit_mm::ContiguousCodeMemoryManager` for the EMIT-INLINE
        // fix that closed the SIGSEGV on >= 4-closure dispatch tables.
        self.builder.position_at_end(cur_bb);
        let cases: Vec<(IntValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> = case_bbs
            .iter()
            .enumerate()
            .map(|(i, bb)| (i32_t.const_int(i as u64, false), *bb))
            .collect();
        self.builder
            .build_switch(fn_idx, default_bb, &cases)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure switch: {e}")))?;

        // Per-case body: direct call to the matching lambda fn.
        for (slot, case_bb) in case_bbs.iter().enumerate() {
            self.builder.position_at_end(*case_bb);
            let callee = self.closure_fn_table[slot];
            // AOT-4: a module may host lambdas of *different* arities
            // (W18 binds a 1-arg filter predicate alongside a 2-arg
            // recursive `is_prime`). The `fn_idx` switch enumerates
            // every lambda slot, but only the slot whose arity matches
            // this call site's `param_tys` can be selected at runtime
            // (the handle's `fn_table_idx` always points at the lambda
            // the predicate / recursion actually targets). Statically
            // emitting a call into a wrong-arity callee would make the
            // LLVM verifier reject the module ("Incorrect number of
            // arguments"). Guard each case: when the callee's arity
            // (state + captures_ptr + user params) disagrees with this
            // call's shape, the case is dead — emit `llvm.trap` +
            // `unreachable` instead of the ill-typed call.
            //
            // EMIT-INLINE fix: arity alone is *not* enough to decide a
            // case is live. A module can host several same-arity lambdas
            // whose *signatures* still differ — most importantly in their
            // return type. The W16 3-partition kernel binds a recursive
            // `List<Int> -> Int` where-helper (`closure_fn_table[0]`,
            // returns i64) *alongside* the `(x: Int) -> Bool` filter
            // predicates (return i32); both have arity 3 (`state`,
            // `captures_ptr`, one user param). A predicate call site's
            // `ret_slot` is i32, so emitting a real `call i64 @helper`
            // in that case would `store i64 <result>, ptr <i32 slot>` —
            // an 8-byte write into a 4-byte entry-block alloca that
            // clobbers the adjacent slot, plus a recursive call into the
            // helper with the predicate's i64 element coerced into the
            // helper's `List<Int>` handle param (a wild arena offset).
            // The case is dynamically dead (the handle's `fn_table_idx`
            // only ever selects a lambda whose signature matches the call
            // site) but its *static* presence is memory-unsafe and gives
            // the optimiser license to miscompile. Trap the case as dead
            // whenever the callee's return-type width disagrees with this
            // call site's expected return width, not just on arity.
            let want_arity = 2 + user_args.len();
            let want_ret_llvm = match ret_ty {
                IrType::Null => None,
                other => Some(self.ir_ty_to_llvm_int(other)?.get_bit_width()),
            };
            let have_ret_llvm = callee.get_type().get_return_type().and_then(|t| match t {
                inkwell::types::BasicTypeEnum::IntType(it) => Some(it.get_bit_width()),
                _ => None,
            });
            let signature_compatible =
                callee.count_params() as usize == want_arity && have_ret_llvm == want_ret_llvm;
            if !signature_compatible {
                let trap = self.llvm_trap_fn.ok_or_else(|| {
                    LlvmError::Codegen(
                        "CallClosure incompatible-signature case: llvm.trap not declared".into(),
                    )
                })?;
                self.builder
                    .build_call(trap, &[], "cc_sig_trap")
                    .map_err(|e| LlvmError::Codegen(format!("CallClosure sig trap call: {e}")))?;
                self.builder
                    .build_unreachable()
                    .map_err(|e| LlvmError::Codegen(format!("CallClosure sig unreachable: {e}")))?;
                continue;
            }
            // Build args: (state, captures_ptr, user_args...). Coerce
            // each user arg to *this callee's* declared LLVM param width
            // (param 0 = state ptr, param 1 = captures_ptr, params 2.. =
            // the user args) so a same-arity sibling lambda with a
            // different param width still type-checks in its (dead) case.
            let callee_param_tys = callee.get_type().get_param_types();
            let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
                Vec::with_capacity(2 + user_args.len());
            call_args.push(state_ptr.into());
            call_args.push(captures_ptr.into());
            for (i, v) in user_args.iter().enumerate() {
                let want_width = match callee_param_tys.get(2 + i) {
                    Some(inkwell::types::BasicMetadataTypeEnum::IntType(t)) => t.get_bit_width(),
                    _ => v.get_type().get_bit_width(),
                };
                let have_width = v.get_type().get_bit_width();
                let coerced = if have_width == want_width {
                    *v
                } else {
                    let target_ty = if want_width == 64 {
                        self.ctx.i64_type()
                    } else {
                        self.ctx.i32_type()
                    };
                    if have_width < want_width {
                        self.builder
                            .build_int_z_extend(*v, target_ty, "cc_arg_zext")
                            .map_err(|e| {
                                LlvmError::Codegen(format!("CallClosure arg #{i} zext: {e}"))
                            })?
                    } else {
                        self.builder
                            .build_int_truncate(*v, target_ty, "cc_arg_trunc")
                            .map_err(|e| {
                                LlvmError::Codegen(format!("CallClosure arg #{i} trunc: {e}"))
                            })?
                    }
                };
                call_args.push(coerced.into());
            }
            let name = self.next_name("cc_call");
            let call_site = self
                .builder
                .build_call(callee, &call_args, &name)
                .map_err(|e| LlvmError::Codegen(format!("CallClosure call: {e}")))?;
            if let Some(slot) = ret_slot {
                let v = match call_site.try_as_basic_value() {
                    inkwell::values::ValueKind::Basic(v) => v,
                    inkwell::values::ValueKind::Instruction(_) => {
                        return Err(LlvmError::Codegen(
                            "CallClosure: callee returned void but ret_ty != Null".into(),
                        ));
                    }
                };
                self.builder
                    .build_store(slot, v)
                    .map_err(|e| LlvmError::Codegen(format!("CallClosure ret store: {e}")))?;
            }
            self.builder
                .build_unconditional_branch(post_bb)
                .map_err(|e| LlvmError::Codegen(format!("CallClosure case br: {e}")))?;
        }

        // Default block: invoke llvm.trap and fall through to an
        // `unreachable` so the verifier accepts the terminator.
        self.builder.position_at_end(default_bb);
        let trap = self.llvm_trap_fn.ok_or_else(|| {
            LlvmError::Codegen("CallClosure default trap: llvm.trap not declared".into())
        })?;
        self.builder
            .build_call(trap, &[], "cc_trap")
            .map_err(|e| LlvmError::Codegen(format!("CallClosure trap call: {e}")))?;
        self.builder
            .build_unreachable()
            .map_err(|e| LlvmError::Codegen(format!("CallClosure unreachable: {e}")))?;

        // Continue with the post block; pop the result slot into the
        // operand stack.
        self.builder.position_at_end(post_bb);
        if let Some(slot) = ret_slot {
            let llvm_ty = self.ir_ty_to_llvm_int(ret_ty)?;
            let name = self.next_name("cc_ret_load");
            let v = self
                .builder
                .build_load(llvm_ty, slot, &name)
                .map_err(|e| LlvmError::Codegen(format!("CallClosure ret load: {e}")))?
                .into_int_value();
            self.push(v, ret_ty);
        }
        Ok(())
    }

    /// Phase F.W7 self-recursion fast path companion to
    /// [`Self::emit_call_closure`]. Emits a single `call` instruction
    /// straight against `callee` with `(state, captures_ptr_arg,
    /// args...)` — no handle deref, no switch, no trap branch. The
    /// caller has already proven (via [`Provenance::OwnCaptureHandle`])
    /// that the runtime handle's fields satisfy the call ABI.
    ///
    /// Width-coerces each user arg the same way the slow-path
    /// dispatcher does, then pushes the call result back onto the
    /// operand stack (when the callee's return type isn't `Null`).
    #[allow(clippy::too_many_arguments)]
    fn emit_call_closure_direct(
        &mut self,
        callee: FunctionValue<'ctx>,
        state_ptr: PointerValue<'ctx>,
        captures_ptr: IntValue<'ctx>,
        mut user_args: Vec<IntValue<'ctx>>,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        // Width-coerce each user arg to the callee's declared shape
        // (mirrors the slow-path dispatcher's `cc_arg_zext` /
        // `cc_arg_trunc` pass).
        for (i, (slot, want_ty)) in user_args.iter_mut().zip(param_tys.iter()).enumerate() {
            let want_width = match *want_ty {
                IrType::I64 => 64,
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
                IrType::F64 => 64,
            };
            let have_width = slot.get_type().get_bit_width();
            if have_width != want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                let coerced = if have_width < want_width {
                    self.builder
                        .build_int_z_extend(*slot, target_ty, "ccd_arg_zext")
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(direct) arg #{i} zext: {e}"))
                        })?
                } else {
                    self.builder
                        .build_int_truncate(*slot, target_ty, "ccd_arg_trunc")
                        .map_err(|e| {
                            LlvmError::Codegen(format!("CallClosure(direct) arg #{i} trunc: {e}"))
                        })?
                };
                *slot = coerced;
            }
        }

        // Build the LLVM call arg list `(state, captures_ptr_arg,
        // user_args...)` matching `declare_lambda_function`'s signature.
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> =
            Vec::with_capacity(2 + user_args.len());
        call_args.push(state_ptr.into());
        call_args.push(captures_ptr.into());
        for v in &user_args {
            call_args.push((*v).into());
        }
        let name = self.next_name("ccd_call");
        let call_site = self
            .builder
            .build_call(callee, &call_args, &name)
            .map_err(|e| LlvmError::Codegen(format!("CallClosure(direct) call: {e}")))?;
        if !matches!(ret_ty, IrType::Null) {
            let v = match call_site.try_as_basic_value() {
                inkwell::values::ValueKind::Basic(v) => v,
                inkwell::values::ValueKind::Instruction(_) => {
                    return Err(LlvmError::Codegen(
                        "CallClosure(direct): callee returned void but ret_ty != Null".into(),
                    ));
                }
            };
            let v_int = match v {
                BasicValueEnum::IntValue(i) => i,
                BasicValueEnum::FloatValue(f) => self
                    .builder
                    .build_bit_cast(f, self.ctx.i64_type(), "ccd_ret_bitcast")
                    .map_err(|e| {
                        LlvmError::Codegen(format!("CallClosure(direct) ret bitcast: {e}"))
                    })?
                    .into_int_value(),
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "CallClosure(direct): callee returned unsupported BasicValue {other:?}"
                    )));
                }
            };
            self.push(v_int, ret_ty);
        }
        Ok(())
    }

    /// Map an `IrType` to the LLVM int type used for the operand stack
    /// representation. Used by `Op::MakeClosure` capture reads and
    /// `Op::CallClosure` return loads.
    fn ir_ty_to_llvm_int(&self, ty: IrType) -> Result<inkwell::types::IntType<'ctx>, LlvmError> {
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

    /// Lower `Op::StrConcatN { operand_count }`. Pops N i32 arena
    /// offsets, sums their `[len: u32]` headers, allocates one scratch
    /// record sized `total + 4`, stamps the header, then memcpys each
    /// operand's payload at the running cursor. Pushes the resulting
    /// i32 offset. Mirrors cranelift's `emit_str_concat_n`.
    fn emit_str_concat_n(&mut self, ip_hint: &str, operand_count: u32) -> Result<(), LlvmError> {
        if operand_count < 2 {
            return Err(LlvmError::Codegen(format!(
                "Op::StrConcatN with operand_count={operand_count} (expected >= 2)"
            )));
        }
        let n = operand_count as usize;
        let i32_t = self.ctx.i32_type();
        // Pop N i32 offsets; reverse so source-order matches stack-
        // order (deepest leaf is the first operand).
        let mut offs: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for _ in 0..n {
            offs.push(self.pop_int(ip_hint)?);
        }
        offs.reverse();
        // Load each operand's `[len: u32]` header once.
        let mut lens: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for off in &offs {
            let addr = self.arena_addr_i32(*off)?;
            let name = self.next_name("strconcat_len");
            let l = self
                .builder
                .build_load(i32_t, addr, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN len load: {e}")))?
                .into_int_value();
            lens.push(l);
        }
        // total_len = Σ lens.
        let mut total_len = lens[0];
        for v in &lens[1..] {
            let name = self.next_name("strconcat_sumlen");
            total_len = self
                .builder
                .build_int_add(total_len, *v, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN sum: {e}")))?;
        }
        // record_size = total_len + 4 (header).
        let four = i32_t.const_int(4, false);
        let name = self.next_name("strconcat_recsize");
        let record_size = self
            .builder
            .build_int_add(total_len, four, &name)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN record_size: {e}")))?;
        // Allocate the scratch record.
        self.emit_alloc_scratch_common(record_size)?;
        let base_off = self.pop_int(ip_hint)?;
        // Write header: i32.store(base, total_len).
        let base_abs = self.arena_addr_i32(base_off)?;
        self.builder
            .build_store(base_abs, total_len)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN header store: {e}")))?;
        // Walk operands in source order, copying payloads at the
        // running cursor.
        let name = self.next_name("strconcat_cursor0");
        let mut cursor_off = self
            .builder
            .build_int_add(base_off, four, &name)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN cursor init: {e}")))?;
        for i in 0..n {
            let len = lens[i];
            let name = self.next_name("strconcat_srcoff");
            let src_off_payload = self
                .builder
                .build_int_add(offs[i], four, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN src off: {e}")))?;
            let dst_ptr = self.arena_addr_i32(cursor_off)?;
            let src_ptr = self.arena_addr_i32(src_off_payload)?;
            let i64_t = self.ctx.i64_type();
            let name = self.next_name("strconcat_lenzext");
            let len64 = self
                .builder
                .build_int_z_extend(len, i64_t, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN len zext: {e}")))?;
            self.builder
                .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN memcpy: {e}")))?;
            let name = self.next_name("strconcat_cursornext");
            cursor_off = self
                .builder
                .build_int_add(cursor_off, len, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN cursor bump: {e}")))?;
        }
        // Push the resulting record offset.
        self.push(base_off, IrType::String);
        Ok(())
    }

    /// Lower `Op::Add(IrType::String)` with the W3 reduce-accumulator
    /// fast path. Pops `[lhs_off, rhs_off]` (i32 arena offsets); emits a
    /// runtime branch that picks between:
    ///
    /// * **In-place append (fast)** — when `lhs` is the most recent
    ///   scratch allocation (`lhs_off + 4 + lhs_len == scratch_base +
    ///   scratch_cursor`), extend the existing record by `rhs_len`
    ///   bytes. Updates the header in-place, copies only the rhs
    ///   payload, bumps `scratch_cursor` by `rhs_len`. Result offset =
    ///   `lhs_off`. This is the W3 hot loop's steady-state path: every
    ///   iteration's freshly-built accumulator is the most recent
    ///   allocation, so concatenating one more byte costs O(1) (a
    ///   single byte store + cursor bump) instead of the historical
    ///   O(N) re-copy of the running accumulator.
    /// * **Full alloc + copy (slow)** — when the lhs sits somewhere
    ///   else in the arena (e.g. const-pool literal, scratch alloc
    ///   from a different sub-expression). Replicates the historical
    ///   `concat` stdlib body: allocate `lhs_len + rhs_len + 4` bytes
    ///   of scratch, stamp the header, memcpy both payloads. Result
    ///   offset = the freshly-allocated base.
    ///
    /// The two arms merge at a phi node, and the resulting i32 offset
    /// is pushed back tagged as [`IrType::String`].
    ///
    /// ## Correctness ground
    ///
    /// The in-place mutation overwrites both:
    /// * the existing `[len: u32]` header at `[lhs_off..lhs_off+4]`,
    /// * the bytes immediately past the existing payload, at
    ///   `[lhs_off+4+lhs_len .. lhs_off+4+lhs_len+rhs_len]`.
    ///
    /// The guard `lhs_off + 4 + lhs_len == scratch_base +
    /// scratch_cursor` ensures the bytes past the payload are inside
    /// the unallocated scratch tail — no other live data sits there.
    /// The result offset shares its base with the lhs, so any
    /// subsequent reader that previously held `lhs_off` would now see
    /// the longer record — but in the reduce pattern the lhs slot
    /// (`acc`) is immediately overwritten by the `LetSet` that follows
    /// `Op::Add(String)`, so no stale alias remains.
    ///
    /// The fast path also keeps `scratch_cursor` advanced by exactly
    /// the same byte count that the slow path would have advanced it
    /// for the fresh record (`rhs_len` extra bytes vs `lhs_len +
    /// rhs_len + 4` extra bytes for a full copy), so the arena's
    /// out-of-bounds budget is *strictly tighter* than the historical
    /// path — there is no new failure mode where the fast path
    /// exceeds the arena while the slow path would have fit.
    fn emit_str_add_inplace_or_concat(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::Add(String) outside buffer-protocol entry shape (no arena_base)".into(),
            )
        })?;
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::Add(String) outside buffer-protocol entry shape (no state)".into(),
            )
        })?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();
        let i64_t = self.ctx.i64_type();

        // Pop in reverse order: stack is `[lhs, rhs]`, top is rhs.
        // Phase L W3: keep the TypedValue so we can read provenance
        // (notably `Provenance::ConstString { len, first_byte }`) to
        // pick the const-len fast path below. LLVM cannot prove the
        // const length on its own — the rhs offset is a runtime i32
        // that happens to point into the const-pool prefix, and the
        // `[len]` header at that offset is reloaded every iteration
        // because the in-place append's header store at `lhs_addr`
        // aliases against it from the optimiser's point of view.
        let rhs_tv = self.pop(ip_hint)?;
        let lhs_tv = self.pop(ip_hint)?;
        let rhs_off = rhs_tv.val;
        let lhs_off = lhs_tv.val;
        let rhs_const_len: Option<(u32, Option<u8>)> = match rhs_tv.prov {
            Provenance::ConstString { len, first_byte } => Some((len, first_byte)),
            _ => None,
        };
        // SAFETY: when the *lhs* is sourced from `Op::ConstString` the
        // operand points into the per-module const-pool prefix (read-
        // only). Allowing the in-place fast path to fire in that case
        // would write the new `[len]` header — and the appended payload
        // — *into the const pool*, corrupting every subsequent
        // `Op::ConstString` load. We deliberately do **not** propagate
        // const-len knowledge for the lhs: keep the runtime `[len]`
        // load + the `lhs_end == scratch_end` runtime guard. In
        // practice the const-pool record sits at a fixed prefix offset
        // and the scratch tail is past every literal, so the guard
        // mismatches and the slow path (fresh scratch alloc + double
        // memcpy) takes over for the W3 reduce's first iteration
        // (`acc = "" + "a"`). The const-len optimisation is restricted
        // to the rhs slot.
        let lhs_const_len: Option<u32> = None;
        // Bind to silence the unused-binding lint while keeping the
        // structural symmetry with `rhs_const_len`.
        let _ = lhs_tv;

        // Load lhs.len and rhs.len from header word at offset 0 of
        // each record. Phase L W3: when the operand is known
        // const-string (provenance carries the literal byte length),
        // skip the per-iter `[len]` header load and feed LLVM an
        // i32 const — this removes the alias hazard between the
        // in-place store at `lhs_addr` and the rhs header read.
        let lhs_addr = self.arena_addr_i32(lhs_off)?;
        let lhs_len = if let Some(len) = lhs_const_len {
            i32_t.const_int(u64::from(len), false)
        } else {
            self.builder
                .build_load(i32_t, lhs_addr, "stradd_lhs_len")
                .map_err(|e| LlvmError::Codegen(format!("Add(String) lhs len load: {e}")))?
                .into_int_value()
        };
        let rhs_len = if let Some((len, _)) = rhs_const_len {
            i32_t.const_int(u64::from(len), false)
        } else {
            let rhs_addr = self.arena_addr_i32(rhs_off)?;
            self.builder
                .build_load(i32_t, rhs_addr, "stradd_rhs_len")
                .map_err(|e| LlvmError::Codegen(format!("Add(String) rhs len load: {e}")))?
                .into_int_value()
        };

        // Read scratch_base + scratch_cursor from the arena state.
        let scratch_cur_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_CURSOR), false)],
                    "stradd_scratch_cur_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_cur GEP: {e}")))?
        };
        let scratch_cur = self
            .builder
            .build_load(i32_t, scratch_cur_gep, "stradd_scratch_cur")
            .map_err(|e| LlvmError::Codegen(format!("scratch_cur load: {e}")))?
            .into_int_value();
        let scratch_base_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_BASE), false)],
                    "stradd_scratch_base_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_base GEP: {e}")))?
        };
        let scratch_base = self
            .builder
            .build_load(i32_t, scratch_base_gep, "stradd_scratch_base")
            .map_err(|e| LlvmError::Codegen(format!("scratch_base load: {e}")))?
            .into_int_value();

        // lhs_end = lhs_off + 4 + lhs_len
        let four = i32_t.const_int(4, false);
        let lhs_off_plus_4 = self
            .builder
            .build_int_add(lhs_off, four, "stradd_lhs_off_plus4")
            .map_err(|e| LlvmError::Codegen(format!("stradd lhs+4: {e}")))?;
        let lhs_end = self
            .builder
            .build_int_add(lhs_off_plus_4, lhs_len, "stradd_lhs_end")
            .map_err(|e| LlvmError::Codegen(format!("stradd lhs_end: {e}")))?;
        // scratch_end = scratch_base + scratch_cursor
        let scratch_end = self
            .builder
            .build_int_add(scratch_base, scratch_cur, "stradd_scratch_end")
            .map_err(|e| LlvmError::Codegen(format!("stradd scratch_end: {e}")))?;
        let is_tail = self
            .builder
            .build_int_compare(IntPredicate::EQ, lhs_end, scratch_end, "stradd_is_tail")
            .map_err(|e| LlvmError::Codegen(format!("stradd cmp: {e}")))?;

        let fast_bb = self.ctx.append_basic_block(self.func, "stradd_fast");
        let slow_bb = self.ctx.append_basic_block(self.func, "stradd_slow");
        let merge_bb = self.ctx.append_basic_block(self.func, "stradd_merge");
        self.builder
            .build_conditional_branch(is_tail, fast_bb, slow_bb)
            .map_err(|e| LlvmError::Codegen(format!("stradd branch: {e}")))?;

        // --- fast path: in-place append ---
        self.builder.position_at_end(fast_bb);
        let total_len_fast = self
            .builder
            .build_int_add(lhs_len, rhs_len, "stradd_fast_total")
            .map_err(|e| LlvmError::Codegen(format!("stradd fast total: {e}")))?;
        // store updated header
        self.builder
            .build_store(lhs_addr, total_len_fast)
            .map_err(|e| LlvmError::Codegen(format!("stradd fast header store: {e}")))?;
        // Append the rhs payload onto the lhs tail. Phase L W3: when
        // the rhs is a known const string (the dominant W3 reduce
        // shape — `acc + "a"`), specialise the copy:
        //   * len == 1 — emit a single `store i8 byte, ptr` against
        //     the lhs tail; bypasses the memcpy intrinsic entirely
        //     so the LLVM mid-end sees just a one-byte store + cursor
        //     bump (matching `String::push_str("a")`).
        //   * len > 1 — still use `build_memcpy`, but pass an i64
        //     const for the size so LLVM's `expand-memcpy` lowering
        //     unrolls to inline loads/stores instead of an indirect
        //     `callq *memcpy`.
        //   * non-const — historical path: zext runtime rhs_len to
        //     i64 and hand it to memcpy.
        let fast_dst = self.arena_addr_i32(lhs_end)?;
        match rhs_const_len {
            Some((1, Some(byte))) => {
                let byte_const = i8_t.const_int(u64::from(byte), false);
                self.builder
                    .build_store(fast_dst, byte_const)
                    .map_err(|e| {
                        LlvmError::Codegen(format!("stradd fast inline-byte store: {e}"))
                    })?;
            }
            Some((len, _)) => {
                let rhs_payload_off = self
                    .builder
                    .build_int_add(rhs_off, four, "stradd_rhs_payload_off")
                    .map_err(|e| LlvmError::Codegen(format!("stradd rhs payload off: {e}")))?;
                let fast_src = self.arena_addr_i32(rhs_payload_off)?;
                let rhs_len64 = i64_t.const_int(u64::from(len), false);
                self.builder
                    .build_memcpy(fast_dst, 1, fast_src, 1, rhs_len64)
                    .map_err(|e| {
                        LlvmError::Codegen(format!("stradd fast memcpy (const-len): {e}"))
                    })?;
            }
            None => {
                let rhs_payload_off = self
                    .builder
                    .build_int_add(rhs_off, four, "stradd_rhs_payload_off")
                    .map_err(|e| LlvmError::Codegen(format!("stradd rhs payload off: {e}")))?;
                let fast_src = self.arena_addr_i32(rhs_payload_off)?;
                let rhs_len64 = self
                    .builder
                    .build_int_z_extend(rhs_len, i64_t, "stradd_rhs_len64")
                    .map_err(|e| LlvmError::Codegen(format!("stradd rhs_len zext: {e}")))?;
                self.builder
                    .build_memcpy(fast_dst, 1, fast_src, 1, rhs_len64)
                    .map_err(|e| LlvmError::Codegen(format!("stradd fast memcpy: {e}")))?;
            }
        }
        // bump scratch_cursor by rhs_len
        let new_cur = self
            .builder
            .build_int_add(scratch_cur, rhs_len, "stradd_fast_newcur")
            .map_err(|e| LlvmError::Codegen(format!("stradd fast new cur: {e}")))?;
        self.builder
            .build_store(scratch_cur_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("stradd fast cursor store: {e}")))?;
        let fast_end_bb = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| LlvmError::Codegen(format!("stradd fast->merge: {e}")))?;

        // --- slow path: full alloc + double memcpy ---
        self.builder.position_at_end(slow_bb);
        // total_len = lhs_len + rhs_len
        let total_len_slow = self
            .builder
            .build_int_add(lhs_len, rhs_len, "stradd_slow_total")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow total: {e}")))?;
        // record_size = total_len + 4
        let record_size = self
            .builder
            .build_int_add(total_len_slow, four, "stradd_slow_recsize")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow recsize: {e}")))?;
        self.emit_alloc_scratch_common(record_size)?;
        let base_off = self.pop_int(ip_hint)?;
        // write header at base
        let base_addr = self.arena_addr_i32(base_off)?;
        self.builder
            .build_store(base_addr, total_len_slow)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow header store: {e}")))?;
        // memcpy lhs payload to base+4
        let base_plus_4 = self
            .builder
            .build_int_add(base_off, four, "stradd_slow_basep4")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow base+4: {e}")))?;
        let dst1 = self.arena_addr_i32(base_plus_4)?;
        let lhs_payload_off = self
            .builder
            .build_int_add(lhs_off, four, "stradd_slow_lhsp")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow lhsp: {e}")))?;
        let src1 = self.arena_addr_i32(lhs_payload_off)?;
        // Phase L W3: hand LLVM an i64 const memcpy size whenever
        // the lhs / rhs comes from `Op::ConstString` so the
        // `expand-memcpy` lowering can unroll to inline stores
        // instead of an indirect `callq *memcpy`. Falls back to the
        // historical zext path for non-const operands.
        let lhs_len64: IntValue<'ctx> = if let Some(len) = lhs_const_len {
            i64_t.const_int(u64::from(len), false)
        } else {
            self.builder
                .build_int_z_extend(lhs_len, i64_t, "stradd_slow_lhs64")
                .map_err(|e| LlvmError::Codegen(format!("stradd slow lhs_len zext: {e}")))?
        };
        self.builder
            .build_memcpy(dst1, 1, src1, 1, lhs_len64)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow lhs memcpy: {e}")))?;
        // memcpy rhs payload to base+4+lhs_len
        let lhs_dst_cursor = self
            .builder
            .build_int_add(base_plus_4, lhs_len, "stradd_slow_cur2")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow cur2: {e}")))?;
        let dst2 = self.arena_addr_i32(lhs_dst_cursor)?;
        let rhs_payload_off2 = self
            .builder
            .build_int_add(rhs_off, four, "stradd_slow_rhsp")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow rhsp: {e}")))?;
        let src2 = self.arena_addr_i32(rhs_payload_off2)?;
        let rhs_len64_slow: IntValue<'ctx> = if let Some((len, _)) = rhs_const_len {
            i64_t.const_int(u64::from(len), false)
        } else {
            self.builder
                .build_int_z_extend(rhs_len, i64_t, "stradd_slow_rhs64")
                .map_err(|e| LlvmError::Codegen(format!("stradd slow rhs_len zext: {e}")))?
        };
        self.builder
            .build_memcpy(dst2, 1, src2, 1, rhs_len64_slow)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow rhs memcpy: {e}")))?;
        let slow_end_bb = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow->merge: {e}")))?;

        // --- merge: phi of lhs_off / base_off ---
        self.builder.position_at_end(merge_bb);
        let phi = self
            .builder
            .build_phi(i32_t, "stradd_result")
            .map_err(|e| LlvmError::Codegen(format!("stradd phi: {e}")))?;
        let lhs_off_val: BasicValueEnum<'ctx> = lhs_off.into();
        let base_off_val: BasicValueEnum<'ctx> = base_off.into();
        phi.add_incoming(&[(&lhs_off_val, fast_end_bb), (&base_off_val, slow_end_bb)]);
        let result = phi.as_basic_value().into_int_value();
        // arena_base_ptr is referenced implicitly inside arena_addr_i32;
        // bind it to silence the borrow checker.
        let _ = arena_base_ptr;
        self.push(result, IrType::String);
        Ok(())
    }

    /// Lower `Op::StoreField { ty }` for pointer-indirect types
    /// (`String`, `ListInt`, `ListFloat`, `ListBool`). Pops the source
    /// arena offset, copies the `[len:u32 LE][payload]` record into
    /// the output buffer's tail region (`out_ptr + tail_cursor`),
    /// writes `tail_cursor` (buffer-relative offset of the new record)
    /// into the fixed-area slot at `offset`, and bumps `tail_cursor`.
    /// Mirrors cranelift's `emit_store_pointer_indirect`.
    fn emit_store_field_pointer_indirect(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField (pointer-indirect) outside buffer entry".into())
        })?;
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField (pointer-indirect): missing state ptr".into())
        })?;
        let src_off_i32 = self.pop_int(ip_hint)?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();
        // Read the record's `[len: u32]` header to size the memcpy.
        let src_abs = self.arena_addr_i32(src_off_i32)?;
        let len_i32 = self
            .builder
            .build_load(i32_t, src_abs, "ptr_indirect_len")
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect len load: {e}")))?
            .into_int_value();
        let record_size = match ty {
            IrType::String => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "string_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("String record_size: {e}")))?
            }
            IrType::ListInt | IrType::ListFloat => {
                // record_size = 8 + 8 * element_count.
                let three = i32_t.const_int(3, false);
                let shifted = self
                    .builder
                    .build_left_shift(len_i32, three, "list_shl")
                    .map_err(|e| LlvmError::Codegen(format!("list shl: {e}")))?;
                let eight = i32_t.const_int(8, false);
                self.builder
                    .build_int_add(shifted, eight, "list_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("list record_size: {e}")))?
            }
            IrType::ListBool => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "listbool_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("listbool record_size: {e}")))?
            }
            _ => {
                return Err(LlvmError::Codegen(format!(
                    "emit_store_field_pointer_indirect: unsupported {ty:?}"
                )));
            }
        };
        // Pick the alignment for the tail bump. String / ListBool stay
        // 4-aligned (the leading u32 length); ListInt / ListFloat need
        // 8 so the i64 / f64 payload that follows is aligned.
        let align: u32 = match ty {
            IrType::String | IrType::ListBool => 4,
            IrType::ListInt | IrType::ListFloat => 8,
            _ => unreachable!(),
        };
        // Tail bump: aligned = align_up(cur, align); new_cur = aligned + record_size.
        let tail_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                    "tail_cursor_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("tail_cursor GEP: {e}")))?
        };
        let cur = self
            .builder
            .build_load(i32_t, tail_gep, "tail_cursor_pre")
            .map_err(|e| LlvmError::Codegen(format!("tail_cursor load: {e}")))?
            .into_int_value();
        let aligned = if align <= 1 {
            cur
        } else {
            let add = i32_t.const_int(u64::from(align - 1), false);
            let mask_val = !(align - 1);
            let mask = i32_t.const_int(u64::from(mask_val), false);
            let sum = self
                .builder
                .build_int_add(cur, add, "tail_align_sum")
                .map_err(|e| LlvmError::Codegen(format!("tail align add: {e}")))?;
            self.builder
                .build_and(sum, mask, "tail_align_and")
                .map_err(|e| LlvmError::Codegen(format!("tail align and: {e}")))?
        };
        let new_cur = self
            .builder
            .build_int_add(aligned, record_size, "tail_cursor_post")
            .map_err(|e| LlvmError::Codegen(format!("tail cur bump: {e}")))?;
        self.builder
            .build_store(tail_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("tail cursor store: {e}")))?;
        // memcpy(arena_base + out_ptr + aligned, src_abs, record_size).
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let dst_off = self
            .builder
            .build_int_add(out_ptr_i32, aligned, "ptr_indirect_dst_off")
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect dst off: {e}")))?;
        let dst_ptr = self.arena_addr_i32(dst_off)?;
        let i64_t = self.ctx.i64_type();
        let rec64 = self
            .builder
            .build_int_z_extend(record_size, i64_t, "ptr_indirect_rec64")
            .map_err(|e| LlvmError::Codegen(format!("rec64 zext: {e}")))?;
        let _ = arena_base_ptr;
        self.builder
            .build_memcpy(dst_ptr, align, src_abs, 1, rec64)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect memcpy: {e}")))?;
        // Store `aligned` (buffer-relative offset of the new record)
        // into the fixed-area slot at `out_ptr + offset`.
        let slot_off = self
            .builder
            .build_int_add(
                out_ptr_i32,
                i32_t.const_int(u64::from(offset), false),
                "ptr_indirect_slot_off",
            )
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect slot off: {e}")))?;
        let slot_addr = self.arena_addr_i32(slot_off)?;
        self.builder
            .build_store(slot_addr, aligned)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect slot store: {e}")))?;
        // Flag the body so the buffer-protocol epilogue returns the
        // post-bump tail cursor.
        self.needs_tail_cursor = true;
        Ok(())
    }

    /// Lower `Op::Call { fn_index, ... }` by inlining the bundled
    /// stdlib body. Mirrors cranelift's `emit_call_stdlib` — pop the
    /// args, set up an inline frame with an exit-block target, lower
    /// the callee body recursively against the inline frame, then
    /// continue at the exit block with the loaded result on the stack.
    fn emit_call_stdlib(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        let callee = stdlib.get(fn_index as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::Call fn_index {fn_index} outside bundled stdlib (max {})",
                stdlib.len()
            ))
        })?;
        if callee.params.len() != arg_count as usize {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to `{}` declares {arg_count} args but callee has {}",
                callee.name,
                callee.params.len()
            )));
        }
        for (i, (declared, expected)) in callee.params.iter().zip(param_tys.iter()).enumerate() {
            if declared != expected {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call to `{}` arg #{i}: callee expects {declared:?}, IR tags {expected:?}",
                    callee.name
                )));
            }
        }
        // Pop args in reverse so `params[i]` is the i-th declared arg.
        let mut args: Vec<TypedValue<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop(ip_hint)?);
        }
        args.reverse();

        // Pick a let_offset window past any active let slots so the
        // callee's `LetSet 0` lands at `let_offset + 0` and never
        // clashes with the caller's bindings. Cranelift uses
        // `max(idx) + 1`; we do the same by inspecting `let_slots`.
        let let_offset = self
            .let_slots
            .keys()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        // Alloca for the callee's return value. The callee's
        // `Op::Return` stores into this slot then jumps to `exit_bb`;
        // the caller-side load below pushes the value back on the
        // virtual stack.
        let ret_llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match ret_ty {
            IrType::I64 => self.ctx.i64_type().into(),
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool => self.ctx.i32_type().into(),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call ret_ty {other:?} unsupported in inline frame"
                )));
            }
        };
        // Allocate the ret slot in the function's entry block so it
        // stays out of any loop body; mem2reg promotes it on -O2/-O3.
        let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
            LlvmError::Codegen("emit_call_stdlib: function has no entry block".into())
        })?;
        let cur = self.builder.get_insert_block();
        if let Some(first_instr) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first_instr);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let ret_slot = self
            .builder
            .build_alloca(ret_llvm_ty, "call_ret_slot")
            .map_err(|e| LlvmError::Codegen(format!("call ret_slot alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }

        let exit_bb = self.ctx.append_basic_block(self.func, "call_exit");
        let frame = InlineFrame {
            params: args,
            let_offset,
            ret_slot,
            ret_ty,
            exit_bb,
        };
        self.inline_frames.push(frame);
        let body = callee.body_owned();
        let result = self.lower_body(&body);
        // Always pop the frame before returning the error so the emit
        // state stays consistent on failure.
        self.inline_frames.pop();
        result?;

        // After the inline body finishes the current block has either
        // hit `Op::Return` (which terminated via `br exit_bb`) or fell
        // through. If it fell through, branch to exit_bb so the
        // load + push below has a single in-edge.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline call fallthrough: {e}")))?;
        }
        // Position at the exit block and load the result.
        self.builder.position_at_end(exit_bb);
        let name = self.next_name("call_ret_load");
        let v = self
            .builder
            .build_load(ret_llvm_ty, ret_slot, &name)
            .map_err(|e| LlvmError::Codegen(format!("inline call ret load: {e}")))?
            .into_int_value();
        self.push(v, ret_ty);
        Ok(())
    }

    /// Phase F.1: lower `contains(haystack: String, needle: String) ->
    /// Bool` by emitting a direct extern call to
    /// `relon_llvm_str_contains_arena` instead of inlining the bundled
    /// stdlib body. See the `str_helpers` module docs for the ABI and
    /// the rationale (W4 / W4_long gap vs LuaJIT closed by std's
    /// SIMD-backed `str::contains`).
    ///
    /// Operand stack contract: pops `needle_off` (top), then
    /// `haystack_off`. Pushes the i32 0/1 result tagged as
    /// [`IrType::Bool`] so downstream `If` / `BrIf` ops see the same
    /// width the inlined body would have produced.
    fn emit_str_contains_extern(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        // Pop in reverse order: IR pushes `[haystack, needle]`, so the
        // top-of-stack is the needle. We need to materialise the
        // pointers in declaration order (haystack first) for the call,
        // so collect the offsets first and resolve to pointers below.
        let needle_off = self.pop_int(ip_hint)?;
        let haystack_off = self.pop_int(ip_hint)?;
        self.emit_str_contains_extern_with_offsets(ip_hint, haystack_off, needle_off)
    }

    /// Phase H: shared "given already-popped i32 offsets, emit the
    /// extern shim call" backbone. Split out of
    /// [`Self::emit_str_contains_extern`] so the const-needle
    /// fast path can reuse the extern fallback for `needle.len() > 1`
    /// (where the inline byte-scan no longer wins over the shim's
    /// SIMD-backed Two-Way matcher).
    fn emit_str_contains_extern_with_offsets(
        &mut self,
        _ip_hint: &str,
        haystack_off: IntValue<'ctx>,
        needle_off: IntValue<'ctx>,
    ) -> Result<(), LlvmError> {
        // GEP into the cached arena base. Mirrors `emit_load_at_absolute`
        // / `emit_str_concat_n` — both produce `arena_base + off_i32`
        // pointers the inner ops then read through. The shim consumes
        // raw `*const u8` headers, so we hand the GEP result directly.
        let haystack_ptr = self.arena_addr_i32(haystack_off)?;
        let needle_ptr = self.arena_addr_i32(needle_off)?;

        // Declare (or look up) the extern shim. Idempotent so multiple
        // `contains` call sites in the same module share a single
        // declaration — LLVM's verifier rejects duplicate function
        // definitions but happily reuses an existing extern.
        let shim = self.declare_str_contains_extern();

        let call_name = self.next_name("str_contains_extern");
        let call_site = self
            .builder
            .build_call(
                shim,
                &[
                    BasicMetadataValueEnum::PointerValue(haystack_ptr),
                    BasicMetadataValueEnum::PointerValue(needle_ptr),
                ],
                &call_name,
            )
            .map_err(|e| LlvmError::Codegen(format!("str_contains call: {e}")))?;

        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(
                    "relon_llvm_str_contains_arena returned void; expected i32".into(),
                ));
            }
        };
        let ret_i32 = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "relon_llvm_str_contains_arena returned non-int {other:?}"
                )));
            }
        };
        // Bool is encoded as i32 (0 / 1) across the LLVM AOT envelope,
        // matching what the inlined `contains_string_body` would have
        // produced through `Op::Ne(I32)` against `0`. No truncation /
        // sign-extension needed — the shim returns the same 0/1 i32
        // shape downstream `BrIf` / `Eq(Bool)` consumers expect.
        self.push(ret_i32, IrType::Bool);
        Ok(())
    }

    /// Phase H: lower `contains(haystack, "literal") -> Bool` for the
    /// const-needle case detected at the `Op::Call` site.
    ///
    /// Operand stack contract: pops `needle_off` (top — discarded; we
    /// have the literal bytes), then `haystack_off`, pushes the i32
    /// 0/1 result as [`IrType::Bool`]. The needle's arena-record
    /// pointer is unused on the fast paths because we already know
    /// the bytes at compile time.
    ///
    /// Dispatch by needle length:
    /// - `0` — every haystack contains the empty string; push `i32(1)`
    ///   directly. Matches `core::str::contains("")`'s semantics and
    ///   the bundled stdlib body's `p_len == 0 → true` short-circuit.
    /// - `1` — emit an inline byte-scan loop against the cached
    ///   haystack record. LLVM 18's loop vectoriser recognises the
    ///   single-byte equality scan and lowers it to SSE2 `pcmpeqb` +
    ///   `pmovmskb` (the same SIMD memchr LuaJIT exploits via libc).
    ///   Skips the `relon_llvm_str_contains_arena` FFI boundary — no
    ///   IC atomic loads, no register save/restore, no spill of the
    ///   surrounding loop's IV / accumulator. Per-call cost drops
    ///   from ~5 ns (Phase G shim) to ~1.5-2 ns on x86_64. This is
    ///   the hot path for the W4 / W4_long cmp_lua rows (needle =
    ///   `"x"`).
    /// - `> 1` — fall through to the extern shim. The shim's
    ///   `compute_contains` uses `str::contains` with Rust's Two-Way
    ///   matcher; inlining that here would balloon the IR for no
    ///   measured win (the multi-byte case isn't on the W4 / W4_long
    ///   hot loop).
    fn emit_str_contains_const_needle(
        &mut self,
        ip_hint: &str,
        needle_bytes: &[u8],
    ) -> Result<(), LlvmError> {
        // Pop both operands up-front. For `len == 0` / `len == 1` we
        // discard `needle_off` — the inline path reads the needle byte
        // from the source-emitted `needle_bytes` slice. For `len > 1`
        // we forward both offsets to the shim path.
        let needle_off = self.pop_int(ip_hint)?;
        let haystack_off = self.pop_int(ip_hint)?;

        match needle_bytes.len() {
            0 => {
                // Empty needle: always matches. Push `i32(1)` typed as
                // Bool to match the inlined stdlib body's encoding.
                let one = self.ctx.i32_type().const_int(1, false);
                self.push(one, IrType::Bool);
                Ok(())
            }
            1 => self.emit_str_contains_inline_byte(ip_hint, haystack_off, needle_bytes[0]),
            _ => {
                // Multi-byte needle: shim with Two-Way matcher beats a
                // naive open-coded scan. Forward both offsets.
                self.emit_str_contains_extern_with_offsets(ip_hint, haystack_off, needle_off)
            }
        }
    }

    /// Phase H: emit a direct libc `memchr` call for the single-byte
    /// const-needle case. Pushes the i32 0/1 result tagged as
    /// [`IrType::Bool`].
    ///
    /// IR shape (haystack record at `arena_base + haystack_off` carries
    /// `[len_u32 LE][payload bytes]`):
    ///
    /// ```text
    /// hay_len   = load i32, ptr (arena_base + haystack_off)
    /// hay_payld = gep (arena_base + haystack_off + 4)
    /// hay_len64 = zext i32 hay_len -> i64
    /// res_ptr   = call ptr @memchr(ptr hay_payld, i32 needle_byte, i64 hay_len64)
    /// hit       = icmp ne ptr res_ptr, null
    /// result    = zext i1 hit -> i32
    /// ```
    ///
    /// ## Why direct libc memchr instead of an open-coded scan?
    ///
    /// LLVM 18's loop vectoriser refuses to vectorise the open-coded
    /// scan because the inner body has a data-dependent early exit
    /// (`if byte == needle break`). Without vectorisation the W4_long
    /// row's 256-byte haystack would walk byte-by-byte at ~1 ns / byte
    /// — a ~256 ns/iter regression vs the Phase G shim's SIMD-backed
    /// `core::slice::contains(&u8)` (which calls into the `memchr`
    /// crate's `memchr` function, in turn delegating to libc on
    /// Linux). Calling libc `memchr` directly gives us the same SIMD
    /// `pcmpeqb` + `pmovmskb` lowering glibc ships, *without* the
    /// Phase G shim's per-call IC + record-parsing overhead.
    ///
    /// ## Symbol resolution
    ///
    /// `memchr` is in libc, resolved by MCJIT's default `dlsym` lookup
    /// when the symbol is declared with [`Linkage::External`]. No
    /// explicit `engine.add_global_mapping` call is required (the
    /// Phase F.1 shim needed one because its symbol lives inside the
    /// relon-codegen-llvm dylib, which dlsym can't see from MCJIT).
    fn emit_str_contains_inline_byte(
        &mut self,
        _ip_hint: &str,
        haystack_off: IntValue<'ctx>,
        needle_byte: u8,
    ) -> Result<(), LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let four = i32_t.const_int(4, false);
        let needle_arg = i32_t.const_int(u64::from(needle_byte), false);

        // Materialise haystack record header + payload pointer.
        let hay_hdr_ptr = self.arena_addr_i32(haystack_off)?;
        let hay_len_name = self.next_name("strc_inl_haylen");
        let hay_len = self
            .builder
            .build_load(i32_t, hay_hdr_ptr, &hay_len_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline hay_len: {e}")))?
            .into_int_value();
        let payload_off_name = self.next_name("strc_inl_payoff");
        let payload_off = self
            .builder
            .build_int_add(haystack_off, four, &payload_off_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline payload_off: {e}")))?;
        let hay_payload_ptr = self.arena_addr_i32(payload_off)?;
        let hay_len64_name = self.next_name("strc_inl_haylen64");
        let hay_len64 = self
            .builder
            .build_int_z_extend(hay_len, i64_t, &hay_len64_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline hay_len64: {e}")))?;

        // Declare libc `memchr` once per module.
        let memchr_fn = self.declare_libc_memchr();
        let call_name = self.next_name("strc_inl_memchr");
        let call_site = self
            .builder
            .build_call(
                memchr_fn,
                &[
                    BasicMetadataValueEnum::PointerValue(hay_payload_ptr),
                    BasicMetadataValueEnum::IntValue(needle_arg),
                    BasicMetadataValueEnum::IntValue(hay_len64),
                ],
                &call_name,
            )
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline memchr call: {e}")))?;
        let res_ptr_basic = call_site.try_as_basic_value();
        let res_ptr = match res_ptr_basic {
            inkwell::values::ValueKind::Basic(BasicValueEnum::PointerValue(p)) => p,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "memchr returned non-pointer: {other:?}"
                )));
            }
        };
        let null_ptr = ptr_t.const_null();
        let hit_name = self.next_name("strc_inl_hit");
        let hit_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, res_ptr, null_ptr, &hit_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline cmp: {e}")))?;
        let res_name = self.next_name("strc_inl_res");
        let res_v = self
            .builder
            .build_int_z_extend(hit_i1, i32_t, &res_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline zext: {e}")))?;
        self.push(res_v, IrType::Bool);
        Ok(())
    }

    /// Idempotent declaration of libc `memchr`. Returns the cached
    /// `FunctionValue` so callers can issue `build_call` without
    /// re-parsing the signature. MCJIT's default `dlsym` resolver
    /// picks up the libc symbol — no `engine.add_global_mapping` is
    /// required.
    fn declare_libc_memchr(&self) -> FunctionValue<'ctx> {
        const SYM: &str = "memchr";
        if let Some(f) = self.module.get_function(SYM) {
            return f;
        }
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        // memchr signature: const void *memchr(const void *s, int c, size_t n)
        let fn_ty = ptr_t.fn_type(&[ptr_t.into(), i32_t.into(), i64_t.into()], false);
        self.module
            .add_function(SYM, fn_ty, Some(Linkage::External))
    }

    /// Idempotent declaration of the
    /// [`crate::str_helpers::relon_llvm_str_contains_arena`] extern.
    /// Returns the cached `FunctionValue` so callers can issue
    /// `build_call` without re-parsing the signature on every call site.
    fn declare_str_contains_extern(&self) -> FunctionValue<'ctx> {
        let sym = crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL;
        if let Some(f) = self.module.get_function(sym) {
            return f;
        }
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let fn_ty = i32_t.fn_type(&[ptr_t.into(), ptr_t.into()], false);
        self.module
            .add_function(sym, fn_ty, Some(Linkage::External))
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
