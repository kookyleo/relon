//! Phase 2 smoke tests for [`LlvmAotEvaluator::emit_object`] — the
//! build.rs-facing AOT entry the `relon-rs-build` crate drives.
//!
//! These tests exercise the emit pipeline end-to-end (parse, analyze,
//! lower, LLVM emit, ELF write) without going through the linker.
//! The point is to lock in the entry-shape detection, the
//! per-EmittedField metadata, and the const-pool / shim-reference
//! flags. Anything build.rs or the binding generator branches on
//! belongs here.
//!
//! We **don't** link + execute the emitted object here; that path is
//! covered end-to-end by `crates/relon-rs-demo/src/main.rs`. The
//! smoke tests stay focused on the metadata contract so a future
//! emitter change that breaks the shape (silently flipping a buffer
//! source onto the fast path, or vice versa) surfaces as a test
//! failure before it can corrupt a downstream binding.

use std::path::PathBuf;

use relon_codegen_llvm::{EmittedEntryShape, EmittedFieldType, LlvmAotEvaluator};

/// Tiny helper: write the object to a unique tmp path and return the
/// info. Each test minted its own subdirectory so parallel runs
/// don't clobber each other's `.o` files.
fn emit_to_tmp(name: &str, src: &str) -> Result<relon_codegen_llvm::EmitObjectInfo, String> {
    let tmp_dir =
        std::env::temp_dir().join(format!("relon_emit_object_{name}_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("create tmp dir: {e}"))?;
    let out = tmp_dir.join(format!("{name}.o"));
    let symbol = format!("__test_emit_object_{name}");
    LlvmAotEvaluator::emit_object(src, &symbol, &out).map_err(|e| format!("{e:?}"))
}

#[test]
fn int_only_lowers_to_fast_path() {
    let src = "#main(Int n) -> Int\nn * 2\n";
    let info = emit_to_tmp("int_only", src).expect("emit succeeded");
    assert_eq!(info.shape, EmittedEntryShape::FastInt);
    assert_eq!(info.entry_arity, 1);
    assert_eq!(info.param_names, vec!["n".to_string()]);
    // Fast path doesn't ship per-field metadata or const-data —
    // the binding reads args from positional registers.
    assert!(
        info.main_fields.is_empty(),
        "fast path emits no main_fields"
    );
    assert!(
        info.return_fields.is_empty(),
        "fast path emits no return_fields"
    );
    assert_eq!(info.main_root_size, 0);
    assert_eq!(info.return_root_size, 0);
    assert!(info.const_data.is_empty());
    assert!(!info.references_str_contains_shim);
}

#[test]
fn string_param_lowers_to_buffer() {
    let src = "#main(String s) -> Int\nlength(s)\n";
    let info = emit_to_tmp("string_param", src).expect("emit succeeded");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert_eq!(info.entry_arity, 1);
    assert_eq!(info.param_names, vec!["s".to_string()]);
    // String arg arrives as a pointer-indirect slot — 4-byte
    // offset to the tail-area record. The layout pass picks the
    // slot's byte position; we don't pin the exact value (it's an
    // implementation detail of `relon-eval-api::layout`) but we do
    // require the metadata's shape to match.
    assert_eq!(info.main_fields.len(), 1);
    assert_eq!(info.main_fields[0].name, "s");
    assert_eq!(info.main_fields[0].ty, EmittedFieldType::String);
    // Return is `Ret { value: Int }` under the Phase 2 envelope.
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::Int);
    assert!(info.main_root_size > 0);
    assert!(info.return_root_size > 0);
    // `length(s)` doesn't reference the str_contains shim.
    assert!(!info.references_str_contains_shim);
}

#[test]
fn multi_byte_string_contains_references_shim() {
    // Multi-byte const needle escapes the Phase H single-byte
    // `memchr` fast path and falls through to the
    // `relon_llvm_str_contains_arena` extern. We use a 2-byte
    // needle so the dispatch shape is unambiguous — the
    // `references_str_contains_shim` flag is the signal build.rs
    // uses to force-keep the matching `#[no_mangle]` symbol at
    // link time.
    let src = "#main(String s) -> Bool\ns.contains(\"xy\")\n";
    let info = emit_to_tmp("string_contains_multi", src).expect("emit succeeded");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert!(
        info.references_str_contains_shim,
        "multi-byte s.contains(literal) must route through the host shim — \
         build.rs depends on this flag to force-keep the \
         `relon_llvm_str_contains_arena` symbol at link time"
    );
}

#[test]
fn single_byte_string_contains_uses_libc_memchr() {
    // Phase H single-byte const-needle path lowers to libc `memchr`
    // directly and skips the shim declaration. The flag must
    // reflect that — emitting a `-Wl,-u,relon_llvm_str_contains_arena`
    // cargo directive on a binding that doesn't reference the
    // symbol would create a spurious unresolved-symbol link error
    // for downstream users that didn't pull `relon-rs-shims` in.
    let src = "#main(String s) -> Bool\ns.contains(\"x\")\n";
    let info = emit_to_tmp("string_contains_single", src).expect("emit succeeded");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert!(
        !info.references_str_contains_shim,
        "single-byte const-needle `contains` should lower to libc memchr — \
         shouldn't force the shim staticlib in"
    );
}

#[test]
fn bool_return_decodes_correctly() {
    let src = "#main(String s) -> Bool\ns.contains(\"x\")\n";
    let info = emit_to_tmp("bool_return", src).expect("emit succeeded");
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::Bool);
}

#[test]
fn float_param_lowers_to_buffer() {
    // Float marshalling landed in Phase 1 Stage 2.① — a Float-typed
    // `#main` param routes through the buffer protocol with an
    // 8/8-inline f64 slot, mirroring the MCJIT-side marshaller.
    let src = "#main(Float x) -> Float\nx * 2.0\n";
    let info = emit_to_tmp("float_param", src).expect("Float param now supported");
    assert_eq!(info.shape, EmittedEntryShape::Buffer);
    assert_eq!(info.entry_arity, 1);
    assert_eq!(info.main_fields.len(), 1);
    assert_eq!(info.main_fields[0].name, "x");
    assert_eq!(info.main_fields[0].ty, EmittedFieldType::Float);
    assert_eq!(info.return_fields.len(), 1);
    assert_eq!(info.return_fields[0].ty, EmittedFieldType::Float);
    assert!(info.main_root_size > 0);
    assert!(info.return_root_size > 0);
}

#[test]
fn unique_entry_symbols() {
    // Two emit calls with different symbol names must produce two
    // distinct symbols — the build.rs side uses a hash of the
    // source to guarantee uniqueness when two `.relon` sources have
    // the same shape but different bodies.
    let src = "#main(Int n) -> Int\nn + 1\n";
    let info_a = emit_to_tmp("unique_a", src).expect("emit a");
    let info_b = emit_to_tmp("unique_b", src).expect("emit b");
    assert_ne!(info_a.entry_symbol, info_b.entry_symbol);
}

#[test]
fn binding_path_arity_matches_param_names() {
    // The binding generator pairs `param_names[i]` with
    // `main_fields[i]` under the buffer-protocol envelope. The two
    // arrays must therefore agree on length + order — this test
    // pins the contract.
    let src = "#main(String greeting) -> Int\nlength(greeting)\n";
    let info = emit_to_tmp("named_param", src).expect("emit succeeded");
    assert_eq!(info.param_names.len(), info.main_fields.len());
    assert_eq!(info.param_names[0], info.main_fields[0].name);
    let _ = PathBuf::from("unused — silence unused import lint when feature-gated");
    // satisfy unused
}
