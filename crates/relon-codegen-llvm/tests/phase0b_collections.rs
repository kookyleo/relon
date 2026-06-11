//! Phase 0b — collections-family record-construction op coverage for
//! the LLVM-AOT backend.
//!
//! Covers the three record-local construction ops the Phase 0b seam
//! fills in `src/codegen/collections.rs`:
//!
//! * `Op::AllocSubRecord`  — nested branded sub-dict allocation.
//! * `Op::PushRecordBase`  — push the sub-record base for the parent's
//!   pointer-slot store.
//! * `Op::EmitTailRecordFromAbsoluteAddr` — copy a String / List
//!   record into the parent's tail area and push its offset.
//!
//! ## Why these are validated at the codegen layer, not via `run_main`
//!
//! All three ops are only *reachable* from source shapes whose
//! `#main` return type carries a **nested branded sub-record** (for
//! `AllocSubRecord` / `PushRecordBase`) or a **String/List field
//! inside a sub-record** (for `EmitTailRecordFromAbsoluteAddr`). The
//! host-side return *decoder* (`read_value_from_reader`) supports only
//! top-level Int/Float/Bool/Unit/String fields today — it rejects a
//! nested-`Schema` or List return field with `RuntimeError::Unsupported`
//! ("not supported in Phase B"). That limitation is identical on the
//! cranelift golden backend (`cranelift-native: cannot decode field …`)
//! and lives in shared, non-collections files, so a value-level
//! `run_main` three-way diff is not reachable for these ops without a
//! cross-family decoder change.
//!
//! What this file pins instead is observable and faithful:
//!
//! 1. **Op presence** — the source lowers to the target op (a future
//!    lowering change that stops emitting it trips the assert).
//! 2. **Codegen parity** — both the LLVM backend and the cranelift
//!    golden accept the source through `from_source` (the LLVM
//!    lowering emits real IR, not an `unsupported` error).
//! 3. **IR shape** — the emitted LLVM IR contains the concrete
//!    instructions the cranelift port mandates (the tail-cursor bump,
//!    the sub-record / tail-record memcpy, the parent pointer-slot
//!    store), so the op is provably wired rather than a silent no-op.
//!
//! The const-list family (`ConstListInt` / `ConstListFloat` /
//! `ConstListBool` / `ConstListString`) is now wired (the last was
//! closed by W5-P2 — see `tests/w5_p2_list_string.rs`). `DictGetByStringKey`
//! stays `unsupported` (lands in W5-P3); `ListGetByIntIdx` stays
//! trace-recorder-only (static `List<String>` indexing lowers to inline
//! `LoadI32AtAbsolute` addressing, never that op). The assertion below
//! pins that THESE TWO sources (a nested branded dict + a string
//! subrecord) do not themselves smuggle any of those ops — it is not a
//! statement about global backend support.

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_ir::ir::{Op, TaggedOp};

/// Nested branded sub-dict: `Wrap { Point p, Int tag }` returning a
/// `Wrap` whose `p` field is a nested `Point` — exercises
/// `AllocSubRecord` (the nested `Point` sub-record) + `PushRecordBase`
/// (push its base for the parent's pointer slot).
const NESTED_SRC: &str = "#schema Point { Int x: *, Int y: * }\n\
                          #schema Wrap { Point p: *, Int tag: * }\n\
                          #main(Int n) -> Wrap\n\
                          { p: { x: n, y: n + 1 }, tag: n }";

/// String field inside a branded sub-record: the `Inner.s` String
/// value lowers to `ConstString` then `EmitTailRecordFromAbsoluteAddr`
/// (copy the string record into the sub-record's tail area).
const STRING_SUBREC_SRC: &str = "#schema Inner { String s: * }\n\
                                 #schema Outer { Inner inner: *, Int tag: * }\n\
                                 #main(Int n) -> Outer\n\
                                 { inner: { s: \"hi\" }, tag: n }";

/// Lower `src` to IR (non-strict, same options the LLVM backend uses)
/// and flatten every op in every function body, recursing into
/// structured ops, so a test can assert a given op is present.
fn flatten_ops(src: &str) -> Vec<Op> {
    let options = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let lowered = relon_ir::compile(src, &options).expect("frontend compile");
    let mut out = Vec::new();
    for func in &lowered.module.funcs {
        collect(&func.body, &mut out);
    }
    out
}

fn collect(body: &[TaggedOp], out: &mut Vec<Op>) {
    for t in body {
        out.push(t.op.clone());
        match &t.op {
            Op::Block { body, .. } | Op::Loop { body, .. } => collect(body, out),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect(then_body, out);
                collect(else_body, out);
            }
            _ => {}
        }
    }
}

/// Assert both backends accept `src` through `from_source`. The
/// cranelift golden's acceptance is the codegen oracle: if cranelift
/// lowers the op cleanly, the LLVM port must too. Returns the LLVM
/// post-opt IR dump for further shape assertions.
fn assert_codegen_parity(src: &str) -> String {
    AotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("cranelift golden must compile src:\n{src}\nerr: {e:?}"));
    let llvm = LlvmAotEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("llvm backend must compile src:\n{src}\nerr: {e:?}"));
    llvm.emit_ir_dump().to_string()
}

#[test]
fn nested_branded_dict_emits_alloc_sub_record_and_push_base() {
    let ops = flatten_ops(NESTED_SRC);
    assert!(
        ops.iter().any(|o| matches!(o, Op::AllocSubRecord { .. })),
        "NESTED_SRC must lower to Op::AllocSubRecord; ops:\n{ops:#?}"
    );
    assert!(
        ops.iter().any(|o| matches!(o, Op::PushRecordBase { .. })),
        "NESTED_SRC must lower to Op::PushRecordBase; ops:\n{ops:#?}"
    );
}

#[test]
fn nested_branded_dict_codegen_parity_and_shape() {
    let dump = assert_codegen_parity(NESTED_SRC);
    // The buffer-protocol entry inits + reads the tail cursor (the
    // bump-allocator the sub-record alloc threads through), and the
    // epilogue returns the post-bump cursor. The presence of the
    // tail-cursor wiring proves `AllocSubRecord` reserved tail space.
    assert!(
        dump.contains("tail_cursor"),
        "nested-dict IR must wire the tail cursor (AllocSubRecord bump). Dump:\n{dump}"
    );
    // The parent pointer-slot store (`PushRecordBase` value written into
    // the `Wrap.p` slot) and the nested-field stores compose the output
    // pointer in i64, pass through a bounds check, and only then form the
    // `abs_addr*` GEP. The old unguarded GEP name was `record_dst`.
    assert!(
        dump.contains("record_out_ptr64")
            && dump.contains("add nuw nsw i64 %record_out_ptr64")
            && dump.contains("bounds_arena_len")
            && dump.contains("abs_addr"),
        "nested-dict IR must store fields through the checked record-local path \
         (StoreFieldAtRecord + PushRecordBase slot store). Dump:\n{dump}"
    );
}

#[test]
fn string_subrecord_emits_tail_record_from_absolute() {
    let ops = flatten_ops(STRING_SUBREC_SRC);
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::EmitTailRecordFromAbsoluteAddr { .. })),
        "STRING_SUBREC_SRC must lower to Op::EmitTailRecordFromAbsoluteAddr; ops:\n{ops:#?}"
    );
}

#[test]
fn string_subrecord_codegen_parity_and_shape() {
    let dump = assert_codegen_parity(STRING_SUBREC_SRC);
    // `EmitTailRecordFromAbsoluteAddr` copies the `[len][utf8]` string
    // record into the sub-record's tail area via an `llvm.memcpy`, then
    // stamps the copied record's buffer-relative offset into the
    // field slot. The memcpy is the load-bearing instruction the
    // cranelift `emit_tail_record_from_absolute` port mandates.
    assert!(
        dump.contains("llvm.memcpy"),
        "string-subrecord IR must memcpy the string record into the tail \
         (EmitTailRecordFromAbsoluteAddr). Dump:\n{dump}"
    );
    assert!(
        dump.contains("tail_cursor"),
        "string-subrecord IR must wire the tail cursor (tail-record bump). Dump:\n{dump}"
    );
}

/// Negative pin: the const-list and subscript ops the Phase 0b seam
/// deliberately leaves unimplemented must still surface a clean
/// `unsupported` codegen error (not a panic / miscompile) if a source
/// reaches them. We can't synthesise such a source through the public
/// `from_source` surface without the const-pool widening, so this test
/// documents the contract by asserting the stubbed arms are unreachable
/// from the covered sources (no `ConstListInt` etc. leaked into the
/// validated workloads).
#[test]
fn covered_sources_do_not_smuggle_unsupported_ops() {
    for src in [NESTED_SRC, STRING_SUBREC_SRC] {
        let ops = flatten_ops(src);
        for o in &ops {
            assert!(
                !matches!(
                    o,
                    Op::ConstListInt { .. }
                        | Op::ConstListFloat { .. }
                        | Op::ConstListBool { .. }
                        | Op::ConstListString { .. }
                        | Op::ListGetByIntIdx { .. }
                        | Op::DictGetByStringKey { .. }
                ),
                "covered source unexpectedly lowered to a still-unsupported op {o:?}; \
                 src:\n{src}"
            );
        }
    }
}
