//! Phase 2.b lowering rejection tests.
//!
//! End-to-end engine execution moved to `binary_handshake_smoke.rs`
//! when the wasm signature flipped from the v1.beta scalar form
//! (`(i64) -> i64`) to the binary-handshake form. This file keeps the
//! lowering-side rejection coverage so the IR's narrow surface stays
//! locked in:
//!
//! 1. `MissingMain` — bodies with no `#main(...)` are rejected.
//! 2. `UnsupportedTypeInMain` — non-scalar `#main` parameter / return
//!    types still bounce the user with a structured error rather than
//!    a panic.
//! 3. `UnresolvedVariable` — `#main(Int x)` bodies that reference an
//!    undeclared name surface a structured error.

use relon_ir::{lower_workspace_single, LoweringError};

#[test]
fn missing_main_reports_error() {
    let source = "{ val: 1 }";
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let err = lower_workspace_single(&analyzed, &ast).expect_err("lowering should reject");
    assert!(
        matches!(err, LoweringError::MissingMain { .. }),
        "expected MissingMain, got {err:?}"
    );
}

#[test]
fn unsupported_type_in_main_reports_error() {
    let source = "#main(String s) -> String\ns";
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let err = lower_workspace_single(&analyzed, &ast).expect_err("lowering should reject");
    assert!(
        matches!(err, LoweringError::UnsupportedTypeInMain { .. }),
        "expected UnsupportedTypeInMain, got {err:?}"
    );
}

#[test]
fn unresolved_variable_reports_error() {
    // `y` is not declared on `#main`, so the body walk surfaces an
    // UnresolvedVariable rather than producing a stale field offset.
    let source = "#main(Int x) -> Int\ny + 1";
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let err = lower_workspace_single(&analyzed, &ast).expect_err("lowering should reject");
    assert!(
        matches!(err, LoweringError::UnresolvedVariable { ref name, .. } if name == "y"),
        "expected UnresolvedVariable, got {err:?}"
    );
}

#[test]
fn lowering_packages_params_into_canonical_schema() {
    // The lowering pass synthesises a `MainParams` schema with one
    // field per `#main` parameter, preserving declaration order. This
    // shape is what codegen hashes into `relon.abi`, so reordering
    // the params here would shift the wire-level identity of the
    // module.
    let source = "#main(Int a, Float b) -> Int\na";
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let lowered = lower_workspace_single(&analyzed, &ast).expect("lower");

    assert_eq!(lowered.main_schema.name, "MainParams");
    assert_eq!(lowered.main_schema.fields.len(), 2);
    assert_eq!(lowered.main_schema.fields[0].name, "a");
    assert_eq!(lowered.main_schema.fields[1].name, "b");

    assert_eq!(lowered.return_schema.name, "Ret");
    assert_eq!(lowered.return_schema.fields.len(), 1);
    assert_eq!(lowered.return_schema.fields[0].name, "value");
}
