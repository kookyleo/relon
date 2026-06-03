//! Phase 0b — `mem` family seam: `Op::LoadFieldAtAbsolute` lowering.
//!
//! `LoadFieldAtAbsolute { offset, ty }` is the dynamic-base sibling of
//! `LoadField`: it pops an i32 arena-relative address off the operand
//! stack and loads the field at `offset` of type `ty` from
//! `arena_base + addr + offset`. It is emitted by the IR lowering for
//! schema field access — both `self.field` inside a schema method body
//! and chained `obj.sub.leaf` paths in the entry body.
//!
//! ## Why there is no three-way (cranelift / tree-walker) differential
//! here
//!
//! The task brief called for a cranelift-gold-standard differential.
//! That is not achievable for this op:
//!
//! * **Cranelift** does NOT implement `LoadFieldAtAbsolute` either —
//!   `relon-codegen-cranelift::codegen::op_visitor::visit_load_field_at_absolute`
//!   returns `unsupported("LoadFieldAtAbsolute")`. So cranelift cannot
//!   serve as an oracle; feeding it a fixture that uses the op errors
//!   out before producing any result to compare against.
//!
//! * Every *source-level* path that emits `LoadFieldAtAbsolute` first
//!   lifts the schema-typed receiver to an absolute address with
//!   `Op::LoadSchemaPtr` (for an entry-body `o.inner.v`) or routes the
//!   receiver through a method call (`m.method()`), which itself begins
//!   with `LoadSchemaPtr`. `LoadSchemaPtr` lives in the LLVM backend's
//!   `schema` family seam (`codegen/schema.rs::lower_schema_rest`) and
//!   is still unimplemented. Phase 0b's hard constraint forbids this
//!   lane from touching any family other than `mem`, so the schema
//!   prerequisite cannot be supplied here.
//!
//! * The LLVM `codegen` module is private and there is no public
//!   hand-built-buffer-protocol-IR constructor (`from_ir_direct`
//!   rejects buffer-protocol IR; only `from_source` builds it, and that
//!   goes through the frontend which inserts the `LoadSchemaPtr`
//!   prerequisite). So the op cannot be exercised in isolation through
//!   the public surface.
//!
//! What this test therefore pins is the **codegen reachability**: with
//! the `schema` family now also landed (`LoadSchemaPtr`), a schema
//! program that needs `LoadFieldAtAbsolute` *compiles* — `from_source`
//! succeeds, proving both ops lower. The end-to-end *run* is still
//! blocked, but the block is now downstream of codegen, in the shared
//! Phase-B host arg marshaller (`evaluator.rs::write_value_into_builder`
//! only has scalar Int/Float/Bool/Null arms — no Schema-typed `#main`
//! param support yet), NOT in either family's op lowering. The test
//! asserts exactly that boundary: build OK, run fails at the marshalling
//! envelope with no unsupported-op error. When Phase 1 wires schema-arg
//! marshalling, run_main will succeed and this test must be upgraded to
//! assert the tree-walk value (`Int(42)`).

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Source whose `#main` body reads a nested schema field
/// (`o.inner.v`). The IR lowering emits `LoadSchemaPtr { offset: 0 }`
/// (lift `o` to its absolute address) followed by
/// `LoadFieldAtAbsolute { offset: 0, ty: I64 }` (read `.v`).
const NESTED_FIELD_SRC: &str = "\
#schema Inner { Int v: * }
#schema Outer { Inner inner: * }
#main(Outer o) -> Int
o.inner.v";

fn nested_field_args(v: i64) -> HashMap<String, Value> {
    let inner = Value::dict([("v".to_string(), Value::Int(v))]);
    let outer = Value::dict([("inner".to_string(), inner)]);
    let mut args = HashMap::new();
    args.insert("o".to_string(), outer);
    args
}

/// Regression guard for the `mem`-family seam wiring.
///
/// With both `schema` (`LoadSchemaPtr`) and `mem` (`LoadFieldAtAbsolute`)
/// landed, the nested-field program now *compiles*: `from_source`
/// succeeds, proving `LoadFieldAtAbsolute` lowers (it is dispatched into
/// `lower_mem_rest` and emitted, not hitting the unsupported stub). The
/// end-to-end run is still blocked — but downstream of codegen, in the
/// shared Phase-B host arg marshaller, which has no Schema-typed `#main`
/// param arm yet. We assert that exact boundary: build OK; run fails at
/// the marshalling envelope (`schema expects Schema`), with NO
/// unsupported-op error for either family's op.
#[test]
fn load_field_at_absolute_is_wired_blocked_only_by_arg_marshalling() {
    let ev = LlvmAotEvaluator::from_source(NESTED_FIELD_SRC).expect(
        "schema (LoadSchemaPtr) + mem (LoadFieldAtAbsolute) both lower: \
         the nested-field program must compile",
    );
    // The run is blocked solely by the shared Phase-B arg marshaller
    // (no Schema-typed #main param support). When Phase 1 wires schema-
    // arg marshalling, this returns Ok and the assertion below must be
    // upgraded to `assert_eq!(got, Value::Int(42))`.
    let err = ev
        .run_main(nested_field_args(42))
        .expect_err("schema-typed #main arg is not marshalled in Phase B yet");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("schema expects Schema"),
        "block must be the arg-marshalling envelope, not codegen: {msg}"
    );
    assert!(
        !msg.contains("LoadFieldAtAbsolute") && !msg.contains("LoadSchemaPtr"),
        "neither family op may surface as unsupported anymore: {msg}"
    );
}
