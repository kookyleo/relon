//! Build script for the relon-rs end-to-end demo.
//!
//! Drives the build-time compile of every `.relon` source registered
//! below through the LLVM AOT pipeline. The result is one `.o` per
//! source (linked into the final binary via `cargo:rustc-link-arg`)
//! plus a per-source `OUT_DIR/relon_rs/<alias>.rs` bindings file the
//! `include_relon!` macro stitches into `src/main.rs`.

fn main() {
    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR must be set by cargo during build");
    relon_rs_build::Compiler::new()
        .source("src/foo.relon")
        .source("src/bar.relon")
        .source("src/baz.relon")
        .source("src/qux.relon")
        .source("src/quux.relon")
        .opt_level(3)
        .emit_all(&out_dir)
        .expect("relon-rs-build: compile demo sources");

    // The build script itself only needs to re-run when our own
    // source changes. Per-`.relon` rerun hooks are emitted by
    // `emit_all`.
    println!("cargo:rerun-if-changed=build.rs");
}
