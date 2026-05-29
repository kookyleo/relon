//! Expansion + behaviour test for the `include_relon!` proc-macro.
//!
//! `relon-rs-macro` is a function-like proc-macro crate: there is no
//! token output to unit-test in isolation, so the macro is verified by
//! *using* it. Each invocation expands to
//! `include!(concat!(env!("OUT_DIR"), "/relon_rs/<alias>.rs"))`; the
//! crate's `build.rs` plants the matching `<alias>.rs` fixture into
//! `OUT_DIR/relon_rs/` (standing in for the `relon-rs-build` step a real
//! consumer runs). If the macro derives the wrong alias, emits a
//! malformed `include!`, or mis-parses the `as` clause, this test fails
//! to compile — which is exactly the regression we want to catch.
//!
//! Two forms are covered:
//!
//! 1. Default file-stem alias — `include_relon!("src/compute.relon")`
//!    must derive the alias `compute`, pulling in `compute.rs`.
//! 2. Explicit `as` alias — `include_relon!("src/anything.relon" as
//!    aliased)` must honour `aliased`, pulling in `aliased.rs` even
//!    though the file stem is `anything`.

// Form 1: default alias derived from the file stem (`compute`). The
// fixture file `OUT_DIR/relon_rs/compute.rs` defines `compute_main`.
relon_rs_macro::include_relon!("src/compute.relon");

// Form 2: explicit `as` alias overriding the stem. The path stem is
// `anything` but the macro must resolve the alias to `aliased`, pulling
// in `OUT_DIR/relon_rs/aliased.rs` which defines `aliased_double`.
relon_rs_macro::include_relon!("src/anything.relon" as aliased);

#[test]
fn default_stem_alias_includes_matching_bindings() {
    // The function comes from the included fixture; calling it proves
    // the macro derived the `compute` stem and emitted a well-formed
    // `include!` that the compiler resolved.
    assert_eq!(compute_main(41), 42);
    assert_eq!(compute_main(0), 1);
}

#[test]
fn explicit_as_alias_overrides_file_stem() {
    // `aliased_double` only exists in `aliased.rs`; reaching it proves
    // the `as aliased` clause routed the `include!` to the alias-named
    // bindings file rather than the `anything` file stem.
    assert_eq!(aliased_double(21), 42);
    assert_eq!(aliased_double(-3), -6);
}
