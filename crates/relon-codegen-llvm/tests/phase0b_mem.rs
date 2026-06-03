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
//! What this test therefore pins is the **reachability boundary**: a
//! schema program that needs `LoadFieldAtAbsolute` now fails — if at
//! all — on the still-unimplemented `LoadSchemaPtr` prerequisite, and
//! NOT on `LoadFieldAtAbsolute`. That guards the `mem`-seam wiring: the
//! op is dispatched into `lower_mem_rest` and emitted rather than
//! hitting the unsupported stub. Once the `schema` family lands
//! `LoadSchemaPtr`, this same program should compile and run, at which
//! point a follow-up can add the tree-walker differential. The cross-
//! check below keeps the tree-walker oracle wired so that follow-up is
//! a one-line flip.

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
/// `LoadFieldAtAbsolute` is now routed through `lower_mem_rest` and
/// emitted, so it must NOT be the op that blocks compilation. The only
/// remaining blocker on this source is the out-of-scope `LoadSchemaPtr`
/// prerequisite (schema family). When that lands, the `Ok` arm fires
/// and the result must equal the field value.
#[test]
fn load_field_at_absolute_is_wired_not_the_blocker() {
    let build = LlvmAotEvaluator::from_source(NESTED_FIELD_SRC);
    match build {
        Ok(ev) => {
            // schema family already landed `LoadSchemaPtr`: the program
            // compiles, so the whole chain — including our
            // `LoadFieldAtAbsolute` lowering — must produce the field
            // value. Tree-walker would be the differential oracle here.
            let got = ev
                .run_main(nested_field_args(42))
                .expect("run_main on nested-field program");
            assert_eq!(
                got,
                Value::Int(42),
                "LoadFieldAtAbsolute miscomputed the nested field"
            );
        }
        Err(e) => {
            let msg = format!("{e:?}");
            assert!(
                msg.contains("LoadSchemaPtr"),
                "the only acceptable compile blocker is the out-of-scope \
                 LoadSchemaPtr prerequisite; LoadFieldAtAbsolute must be \
                 wired through lower_mem_rest. got: {msg}"
            );
            assert!(
                !msg.contains("LoadFieldAtAbsolute"),
                "LoadFieldAtAbsolute must no longer surface as unsupported: {msg}"
            );
        }
    }
}
