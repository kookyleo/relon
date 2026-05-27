//! Integration tests for the [`relon_rs_build::Compiler`] surface.
//!
//! The Phase 2 tests exercise the build.rs API end-to-end (parse +
//! analyze + lower + LLVM emit + binding-file generation) for the
//! supported entry shapes. We don't actually `cargo build` the
//! generated binding here — that would require materialising a
//! synthetic crate tree, which the demo crate already covers.
//! Instead each test asserts on the generated binding's textual
//! shape (extern signature, `call_buffer_entry` call site, const-
//! data length) so a future shape change has to update both the
//! generator and the test in lockstep.

use std::path::PathBuf;

use relon_rs_build::Compiler;

/// Write `src` to `tmp_dir/<name>.relon`, run the compiler, return the
/// (object_path, binding_path) tuple alongside the binding's text.
fn compile_one(name: &str, src: &str) -> (PathBuf, PathBuf, String) {
    let tmp_dir = std::env::temp_dir().join(format!(
        "relon_rs_build_integration_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let src_path = tmp_dir.join(format!("{name}.relon"));
    std::fs::write(&src_path, src).expect("write source");
    let out_dir = tmp_dir.join("out");
    let out = Compiler::new()
        .source(&src_path)
        .opt_level(3)
        .emit_all(&out_dir)
        .expect("emit_all");
    assert_eq!(out.objects.len(), 1);
    assert_eq!(out.bindings.len(), 1);
    let binding_src = std::fs::read_to_string(&out.bindings[0]).expect("read generated binding");
    (out.objects[0].clone(), out.bindings[0].clone(), binding_src)
}

#[test]
fn int_only_binding_uses_fast_path_signature() {
    let (obj, _binding_path, binding) = compile_one("int_only", "#main(Int n) -> Int\nn * 2\n");
    assert!(obj.exists(), "object file must be written");
    // Fast-path bindings declare a typed `extern "C" fn(i64...) -> i64`
    // and do **not** route through `call_buffer_entry`.
    assert!(
        binding.contains("fn __relon_") && binding.contains("(n: i64) -> i64"),
        "fast path binding must declare typed i64 extern, got:\n{binding}"
    );
    assert!(
        !binding.contains("call_buffer_entry"),
        "fast path binding must not route through `call_buffer_entry`"
    );
    assert!(
        !binding.contains("CONST_DATA"),
        "fast path binding has no const-data section"
    );
}

#[test]
fn string_param_binding_routes_through_call_buffer_entry() {
    let (obj, _binding_path, binding) =
        compile_one("string_param", "#main(String s) -> Int\nlength(s)\n");
    assert!(obj.exists());
    // The buffer-protocol binding signature exposes `&str` to the
    // caller and packs it into an `ArgValue::String(...)` for the
    // marshaller.
    assert!(
        binding.contains("s: &str"),
        "String param binding must expose `&str` to the caller"
    );
    assert!(
        binding.contains("call_buffer_entry"),
        "buffer-protocol binding must route through call_buffer_entry"
    );
    assert!(
        binding.contains("ArgValue :: String") || binding.contains("ArgValue::String"),
        "String args must be wrapped in ArgValue::String"
    );
    assert!(
        binding.contains("RetValue :: Int") || binding.contains("RetValue::Int"),
        "Int return must be unwrapped via RetValue::Int"
    );
    assert!(
        binding.contains("MAIN_FIELDS"),
        "binding must embed per-field metadata as `MAIN_FIELDS`"
    );
}

#[test]
fn string_contains_binding_carries_shim_keep() {
    let (_, _, binding) = compile_one(
        "string_contains",
        "#main(String s) -> Bool\ns.contains(\"x\")\n",
    );
    assert!(binding.contains("RetValue :: Bool") || binding.contains("RetValue::Bool"));
    // The link-arg directive is emitted to cargo (we can't capture
    // it here without driving the full build script), but the
    // binding's existence is enough — the shim ref flag flows out
    // of `EmitObjectInfo` and gates the `-Wl,-u,...` cargo
    // directive in `Compiler::emit_all`. Smoke-test that the
    // binding compiles to the right shape.
    assert!(binding.contains("call_buffer_entry"));
}

#[test]
fn aliased_source_emits_flat_function() {
    let tmp_dir =
        std::env::temp_dir().join(format!("relon_rs_build_aliased_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let src_path = tmp_dir.join("renamed.relon");
    std::fs::write(&src_path, "#main(Int n) -> Int\nn + 1\n").expect("write");
    let out_dir = tmp_dir.join("out");
    let out = Compiler::new()
        .source_as(&src_path, "compute")
        .emit_all(&out_dir)
        .expect("emit_all");
    let binding = std::fs::read_to_string(&out.bindings[0]).expect("read binding");
    // Aliased emission: top-level `pub fn compute(...)`, no module
    // wrapper.
    assert!(
        binding.contains("pub fn compute("),
        "aliased binding must expose top-level `pub fn compute`, got:\n{binding}"
    );
    assert!(
        !binding.contains("pub mod renamed"),
        "aliased binding must not emit the `mod renamed` wrapper"
    );
}

#[test]
fn duplicate_alias_rejected() {
    let tmp_dir = std::env::temp_dir().join(format!("relon_rs_build_dup_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let src_a = tmp_dir.join("dup.relon");
    let src_b = tmp_dir.join("sub/dup.relon");
    std::fs::create_dir_all(src_b.parent().unwrap()).expect("subdir");
    std::fs::write(&src_a, "#main(Int n) -> Int\nn\n").expect("write a");
    std::fs::write(&src_b, "#main(Int n) -> Int\nn\n").expect("write b");
    let out_dir = tmp_dir.join("out");
    let err = Compiler::new()
        .source(&src_a)
        .source(&src_b)
        .emit_all(&out_dir)
        .expect_err("should reject duplicate alias");
    let msg = format!("{err}");
    assert!(
        msg.contains("duplicate") || msg.contains("Duplicate"),
        "expected duplicate alias error, got: {msg}"
    );
}
