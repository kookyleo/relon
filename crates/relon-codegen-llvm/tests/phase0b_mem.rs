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
//! What this test pins is the **full end-to-end path**: with the
//! `schema` family (`LoadSchemaPtr`) and the `mem` family
//! (`LoadFieldAtAbsolute`) both landed, *and* the Phase 0b host-side
//! Schema-typed `#main` arg marshaller wired into
//! `evaluator.rs::write_value_into_builder` (recursive `sub_record` /
//! `finish_sub_record`), the nested-field program both compiles and
//! runs. `#main(Outer o) -> Int : o.inner.v` with `o.inner.v == 42`
//! returns `Value::Int(42)`, matching the tree-walk gold standard.

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

/// End-to-end value assertion for the nested-schema-field path.
///
/// With `schema` (`LoadSchemaPtr`), `mem` (`LoadFieldAtAbsolute`), and
/// the Phase 0b host-side Schema-typed `#main` arg marshaller all wired,
/// `#main(Outer o) -> Int : o.inner.v` runs end to end. The host packs
/// the nested `Outer { inner: Inner { v: 42 } }` into the input buffer
/// (recursive `sub_record` / `finish_sub_record`), `LoadSchemaPtr` lifts
/// `o` to its absolute address, and `LoadFieldAtAbsolute` reads through
/// to `.inner.v`. The result must equal the tree-walk gold standard,
/// `Value::Int(42)`.
#[test]
fn nested_schema_field_runs_end_to_end() {
    let ev = LlvmAotEvaluator::from_source(NESTED_FIELD_SRC).expect(
        "schema (LoadSchemaPtr) + mem (LoadFieldAtAbsolute) both lower: \
         the nested-field program must compile",
    );
    let got = ev
        .run_main(nested_field_args(42))
        .expect("nested-schema #main arg marshals + runs end to end");
    assert_eq!(
        got,
        Value::Int(42),
        "o.inner.v must read back the gold-standard tree-walk value"
    );
}
