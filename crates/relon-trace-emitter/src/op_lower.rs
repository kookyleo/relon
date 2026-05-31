//! Shared per-op cranelift lowering between
//! [`crate::emitter::TraceEmitter`] (standalone trampoline entry) and
//! [`crate::inline_emit::emit_trace_inline`] (at-call-site embed).
//!
//! Historically both paths carried near-identical line-for-line copies
//! of the per-op lowering rules: every new `TraceOp` variant required
//! editing two files, and a `tests/inline_emit_sync_lint.rs` lint kept
//! the two `emit_op` `match` arms in step. P1-12 collapses the genuine
//! duplicates into this module — a thin [`OpLowerer`] trait exposes the
//! shared emit state (cranelift builder, SSA→value map, overflow bits,
//! deopt block, …) and free `lower_*` helpers do the cranelift
//! ins-builder work against any `impl OpLowerer`.
//!
//! ## What stays divergent
//!
//! The two paths still differ on:
//!
//! * `emit_return` — standalone emits `store + return success_i32`
//!   while inline emits `store + jump post_block(v)`.
//! * `emit_mod` / `emit_loop_head` — the standalone path threads
//!   preheader-hoist caches (`hoisted_mod_magic`,
//!   `hoisted_list_len`, …) that the inline path doesn't carry; we keep
//!   the divergent shapes per-impl rather than pollute the shared
//!   helpers with a "do hoist?" predicate.
//! * `emit_guard_op` — needs three disjoint sub-field references
//!   (`ssa_to_value`, `trace.type_info`, `overflow_bits`) into the
//!   running `FunctionBuilder`, which the trait surface can't express
//!   safely. Each impl keeps the 8-line shim that builds the
//!   [`crate::guard_emit::GuardEmitCtx`] from its own fields.
//! * The standalone path lowers `TraceOp::Call` and the str / dict /
//!   list ops to host-helper calls; the inline path returns
//!   `CallNotSupportedInInline` for the same variants. Both are
//!   per-impl arms in the `emit_op` `match` and never reach this
//!   module.
//!
//! The sync invariant from `lib.rs` still applies — every `TraceOp`
//! variant matched in one `emit_op` MUST be matched in the other (real
//! helper OR explicit Err). The lint test still verifies this; what's
//! gone is the duplicated *body* of the per-op helpers.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, BlockArg, InstBuilder, MemFlags};
use cranelift_frontend::FunctionBuilder;

use relon_trace_jit::{CmpKind, SsaVar};

/// Tag for the three-way overflow-checked binary-op family. Shared
/// between the standalone and inline lowering paths.
#[derive(Debug, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
}

/// Common emit-state surface needed by the shared `lower_*` helpers.
///
/// Each emit path (standalone `TraceEmitterState`, inline
/// `InlineEmitterState`) implements this on its concrete struct so the
/// shared helpers can write into the cranelift `FunctionBuilder`,
/// resolve SSAs, and route deopt branches without knowing which path
/// is live.
///
/// The trait deliberately stays narrow: divergent operations (loop
/// head/back, return, guard, Call/str/dict lowering) live as inherent
/// methods on each impl. See the module doc for the divergence
/// rationale.
pub trait OpLowerer {
    /// Concrete error type for this lowering path. The shared helpers
    /// surface "unbound SSA" through [`Self::lookup`]; everything else
    /// they emit is infallible.
    type Err;

    /// Borrow the underlying cranelift `FunctionBuilder` mutably for
    /// the duration of `cb`. Callback shape (rather than returning
    /// `&mut FunctionBuilder<'_>` directly) is forced by cranelift's
    /// nested `FunctionBuilder<'fbc>` lifetime: `&mut T` is invariant
    /// in `T`, so a trait method returning a reborrowed builder can't
    /// carry the impl's concrete `'fbc` lifetime through the inferred
    /// `'_` slot. The closure form lets the compiler pick a fresh
    /// lifetime at every call site.
    fn with_builder<R>(&mut self, cb: impl FnOnce(&mut FunctionBuilder<'_>) -> R) -> R;

    /// Pointer width for the host (i32 on 32-bit targets, i64
    /// otherwise). Today every caller passes i64 but the helpers stay
    /// agnostic so the test surface that exercises a 32-bit pointer ty
    /// keeps working.
    #[allow(dead_code)]
    fn pointer_ty(&self) -> ir::Type;

    /// Shared deopt block. Guard-paired ops `brif` to it on failure
    /// with `(guard_pc: i32, external_pc: i64)` block args.
    fn deopt_block(&self) -> ir::Block;

    /// Packed `u64[]` arg pointer the trace entry receives as its
    /// second ABI param. `LocalGet(_, slot_idx)` loads off this pointer.
    fn input_args_ptr(&self) -> ir::Value;

    /// Resolve an SSA var to its cranelift `Value`. Returns the path-
    /// specific "unbound SSA" error when the recorder emitted an op
    /// whose input never landed in the map.
    fn lookup(&self, var: SsaVar) -> Result<ir::Value, Self::Err>;

    /// Bind a fresh SSA → cranelift `Value` mapping. SSA invariants
    /// guarantee the recorder never re-binds an existing var.
    fn bind(&mut self, var: SsaVar, v: ir::Value);

    /// Stash the i8 overflow bit produced by a `sadd_overflow` /
    /// `ssub_overflow` / `smul_overflow` keyed on the arith op's
    /// destination SSA. Read later by `Guard(ArithOverflow(dst))`.
    fn record_overflow_bit(&mut self, dst: SsaVar, bit: ir::Value);

    /// Construct the path-specific "malformed buffer" error for a
    /// `LocalGet(_, slot_idx)` whose byte offset (`slot_idx * 8`)
    /// does not fit cranelift's i32 memory-load offset. The recorder
    /// never produces such a slot for a well-formed trace, so this is
    /// a defensive reject that lets the install pipeline fall back to
    /// a non-trace tier instead of silently loading the wrong slot.
    fn slot_offset_overflow(&self, slot_idx: u32) -> Self::Err;
}

/// Convenience: widen a freshly-built value to i64 width via
/// `uextend` when the source is i32. Used inside helper bodies that
/// already grabbed the builder via [`OpLowerer::with_builder`].
fn widen_to_i64_with(builder: &mut FunctionBuilder<'_>, v: ir::Value) -> ir::Value {
    let ty = builder.func.dfg.value_type(v);
    if ty == I64 {
        v
    } else if ty == I32 {
        builder.ins().uextend(I64, v)
    } else {
        v
    }
}

/// Coerce a cranelift `Value` into i64 width via `uextend` from i32
/// or passthrough when already i64. Pointer-typed values are returned
/// unchanged (treating them as 64-bit on the 64-bit hosts the trace
/// JIT supports today).
///
/// Mirrors the legacy `widen_to_i64` method that lived as a private
/// helper on both `TraceEmitterState` and `InlineEmitterState`.
pub fn widen_to_i64<L: OpLowerer + ?Sized>(this: &mut L, v: ir::Value) -> ir::Value {
    this.with_builder(|b| widen_to_i64_with(b, v))
}

/// Lower an `Add` / `Sub` / `Mul` triple to the cranelift
/// signed-overflow primitives, binding the result SSA and stashing the
/// overflow bit for downstream `Guard(ArithOverflow(dst))` predicates.
pub fn lower_binop_i64<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    a: SsaVar,
    b: SsaVar,
    op: BinOp,
) -> Result<(), L::Err> {
    let va = this.lookup(a)?;
    let vb = this.lookup(b)?;
    let (r, of) = this.with_builder(|b| {
        let widened_a = widen_to_i64_with(b, va);
        let widened_b = widen_to_i64_with(b, vb);
        match op {
            BinOp::Add => b.ins().sadd_overflow(widened_a, widened_b),
            BinOp::Sub => b.ins().ssub_overflow(widened_a, widened_b),
            BinOp::Mul => b.ins().smul_overflow(widened_a, widened_b),
        }
    });
    this.bind(dst, r);
    this.record_overflow_bit(dst, of);
    Ok(())
}

/// Lower a `Div` op: divisor-zero pre-check (`brif` to the shared deopt
/// block on `b == 0`) followed by `sdiv`. Seeds the overflow-bit cache
/// with const-0 — `sdiv` only overflows on `i64::MIN / -1` which the
/// recorder's upstream guards already handle (see the longer rationale
/// on the call site in `emitter::emit_div`).
pub fn lower_div<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    a: SsaVar,
    b: SsaVar,
) -> Result<(), L::Err> {
    let va = this.lookup(a)?;
    let vb = this.lookup(b)?;
    let deopt = this.deopt_block();
    let (r, of_bit) = this.with_builder(|builder| {
        let zero = builder.ins().iconst(I64, 0);
        let nonzero = builder.ins().icmp(IntCC::NotEqual, vb, zero);
        let ok_block = builder.create_block();
        let guard_pc = builder.ins().iconst(I32, 0);
        let external_pc = builder.ins().iconst(I64, 0);
        builder.ins().brif(
            nonzero,
            ok_block,
            &[],
            deopt,
            &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
        );
        builder.seal_block(ok_block);
        builder.switch_to_block(ok_block);

        let r = builder.ins().sdiv(va, vb);
        let of_bit = builder.ins().iconst(I32, 0);
        (r, of_bit)
    });
    this.bind(dst, r);
    this.record_overflow_bit(dst, of_bit);
    Ok(())
}

/// Lower a `Mod` op the "plain" way: divisor-zero pre-check, then
/// `i64::MIN srem -1` overflow pre-check, then `srem`. Mirrors the
/// inline path exactly; the standalone path layers preheader-hoist
/// caches plus a magic-multiplier fast path on top of this in its own
/// `emit_mod`.
pub fn lower_mod_plain<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    a: SsaVar,
    b: SsaVar,
) -> Result<(), L::Err> {
    let va = this.lookup(a)?;
    let vb = this.lookup(b)?;
    let deopt = this.deopt_block();

    let (r, of_bit) = this.with_builder(|builder| {
        // (1) Divisor-zero.
        let zero = builder.ins().iconst(I64, 0);
        let nonzero = builder.ins().icmp(IntCC::NotEqual, vb, zero);
        let nonzero_block = builder.create_block();
        let guard_pc = builder.ins().iconst(I32, 0);
        let external_pc = builder.ins().iconst(I64, 0);
        builder.ins().brif(
            nonzero,
            nonzero_block,
            &[],
            deopt,
            &[BlockArg::Value(guard_pc), BlockArg::Value(external_pc)],
        );
        builder.seal_block(nonzero_block);
        builder.switch_to_block(nonzero_block);

        // (2) i64::MIN % -1 (the lone `srem` overflow case).
        let min_v = builder.ins().iconst(I64, i64::MIN);
        let neg_one = builder.ins().iconst(I64, -1);
        let lhs_is_min = builder.ins().icmp(IntCC::Equal, va, min_v);
        let rhs_is_neg_one = builder.ins().icmp(IntCC::Equal, vb, neg_one);
        let overflows = builder.ins().band(lhs_is_min, rhs_is_neg_one);
        let safe_block = builder.create_block();
        let of_guard_pc = builder.ins().iconst(I32, 0);
        let of_external_pc = builder.ins().iconst(I64, 0);
        builder.ins().brif(
            overflows,
            deopt,
            &[
                BlockArg::Value(of_guard_pc),
                BlockArg::Value(of_external_pc),
            ],
            safe_block,
            &[],
        );
        builder.seal_block(safe_block);
        builder.switch_to_block(safe_block);

        let r = builder.ins().srem(va, vb);
        let of_bit = builder.ins().iconst(I32, 0);
        (r, of_bit)
    });
    this.bind(dst, r);
    this.record_overflow_bit(dst, of_bit);
    Ok(())
}

/// Lower a `Cmp` op to `icmp` + `uextend` to i32 so downstream
/// consumers see a uniform i32 boolean SSA.
pub fn lower_cmp<L: OpLowerer + ?Sized>(
    this: &mut L,
    kind: CmpKind,
    dst: SsaVar,
    a: SsaVar,
    b: SsaVar,
) -> Result<(), L::Err> {
    let va = this.lookup(a)?;
    let vb = this.lookup(b)?;
    let cc = match kind {
        CmpKind::Eq => IntCC::Equal,
        CmpKind::Ne => IntCC::NotEqual,
        CmpKind::Lt => IntCC::SignedLessThan,
        CmpKind::Le => IntCC::SignedLessThanOrEqual,
        CmpKind::Gt => IntCC::SignedGreaterThan,
        CmpKind::Ge => IntCC::SignedGreaterThanOrEqual,
    };
    let widened = this.with_builder(|builder| {
        let bit = builder.ins().icmp(cc, va, vb);
        builder.ins().uextend(I32, bit)
    });
    this.bind(dst, widened);
    Ok(())
}

/// Lower a `Load` op to a `load.i64` against the SSA-resolved base.
pub fn lower_load<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    base: SsaVar,
    off: i32,
) -> Result<(), L::Err> {
    let base_v = this.lookup(base)?;
    let r = this.with_builder(|b| b.ins().load(I64, MemFlags::trusted(), base_v, off));
    this.bind(dst, r);
    Ok(())
}

/// Lower a `Store` op to a `store.i64` against the SSA-resolved base,
/// widening the source value to i64 first.
pub fn lower_store<L: OpLowerer + ?Sized>(
    this: &mut L,
    base: SsaVar,
    off: i32,
    src: SsaVar,
) -> Result<(), L::Err> {
    let base_v = this.lookup(base)?;
    let src_v = this.lookup(src)?;
    this.with_builder(|b| {
        let src_v = widen_to_i64_with(b, src_v);
        b.ins().store(MemFlags::trusted(), src_v, base_v, off);
    });
    Ok(())
}

/// Lower a `ConstI32` op to an `iconst.i32`.
pub fn lower_const_i32<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    v: i32,
) -> Result<(), L::Err> {
    let val = this.with_builder(|b| b.ins().iconst(I32, i64::from(v)));
    this.bind(dst, val);
    Ok(())
}

/// Lower a `ConstI64` op to an `iconst.i64`.
pub fn lower_const_i64<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    v: i64,
) -> Result<(), L::Err> {
    let val = this.with_builder(|b| b.ins().iconst(I64, v));
    this.bind(dst, val);
    Ok(())
}

/// Lower a `LocalGet(dst, slot_idx)` op to a load off the entry-fn's
/// packed `u64[]` args pointer at byte offset `slot_idx * 8`.
pub fn lower_local_get<L: OpLowerer + ?Sized>(
    this: &mut L,
    dst: SsaVar,
    slot_idx: u32,
) -> Result<(), L::Err> {
    let off = i64::from(slot_idx) * 8;
    // The cranelift `load` offset is an i32 (`Offset32`). A slot that
    // overflows it cannot be addressed with a simple base+offset load;
    // silently clamping to 0 would load slot 0 instead — a miscompile.
    // Reject the malformed slot so the caller can fall back cleanly.
    let off_i32 = match i32::try_from(off) {
        Ok(v) => v,
        Err(_) => return Err(this.slot_offset_overflow(slot_idx)),
    };
    let args_ptr = this.input_args_ptr();
    let v = this.with_builder(|b| b.ins().load(I64, MemFlags::trusted(), args_ptr, off_i32));
    this.bind(dst, v);
    Ok(())
}
