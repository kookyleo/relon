//! Build script for the relon-rs end-to-end demo.
//!
//! Drives the build-time compile of every `.relon` source registered
//! below through the LLVM AOT pipeline. The result is one `.o` per
//! source (linked into the final binary via `cargo:rustc-link-arg`)
//! plus a per-source `OUT_DIR/relon_rs/<alias>.rs` bindings file the
//! `include_relon!` macro stitches into `src/main.rs`.

fn main() {
    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR must be set by cargo during build");

    // `src/secret.relon` calls the host `#native` fn `clock_add`, gated
    // on the `reads_clock` capability. The closed-world emit resolves
    // the declaration, bakes the `Op::CheckCap(reads_clock)` gate, and
    // links + inlines the host Rust body into the `.o`. The gate is
    // enforced at runtime against the caps mask the consuming binary
    // threads through `SandboxState`: granted → the body runs; denied
    // → a typed `CapabilityDenied` (no crash).
    let mut gate = relon_rs_build::NativeFnGate::default();
    gate.reads_clock = true;
    let clock_add = relon_rs_build::NativeHostFn {
        name: "clock_add".to_string(),
        param_types: vec!["Int".to_string()],
        return_type: "Int".to_string(),
        gate,
        rust_impl: r#"
#[no_mangle]
pub extern "C" fn clock_add(x: i64) -> i64 {
    // Stand-in for a real clock read: deterministic so the demo can
    // assert an exact value. The capability gate is what makes this a
    // privileged call, not the body.
    x.wrapping_add(1_700_000_000)
}
"#
        .to_string(),
    };

    relon_rs_build::Compiler::new()
        .source("src/foo.relon")
        .source("src/bar.relon")
        .source("src/baz.relon")
        .source("src/qux.relon")
        .source("src/quux.relon")
        .source_with_native_fns("src/secret.relon", vec![clock_add])
        .opt_level(3)
        .emit_all(&out_dir)
        .expect("relon-rs-build: compile demo sources");

    // The build script itself only needs to re-run when our own
    // source changes. Per-`.relon` rerun hooks are emitted by
    // `emit_all`.
    println!("cargo:rerun-if-changed=build.rs");
}
