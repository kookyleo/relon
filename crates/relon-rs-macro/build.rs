//! Test-fixture build script for `relon-rs-macro`.
//!
//! The `include_relon!` macro expands to
//! `include!(concat!(env!("OUT_DIR"), "/relon_rs/<alias>.rs"))` — it
//! does not generate the bindings itself; the consuming crate's
//! `relon-rs-build` build step writes them (see the crate-level docs in
//! `src/lib.rs`). To exercise the macro end-to-end in this crate's own
//! integration tests we stand in for that build step here: write a tiny
//! hand-rolled bindings file per alias the tests reference, into the
//! exact `OUT_DIR/relon_rs/<alias>.rs` path the macro emits an
//! `include!` for.
//!
//! These fixtures are inert for downstream consumers: a real dependent
//! crate has its own `OUT_DIR` populated by `relon-rs-build`, never this
//! one. The files only ever feed `tests/include_relon.rs` here.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let bindings_dir = Path::new(&out_dir).join("relon_rs");
    fs::create_dir_all(&bindings_dir).expect("create OUT_DIR/relon_rs");

    // Alias `compute` — exercises the default file-stem derivation
    // (`include_relon!("src/compute.relon")`). Mirrors the shape a real
    // `#main(Int) -> Int` binding would emit: a free function the
    // generated module exposes.
    fs::write(
        bindings_dir.join("compute.rs"),
        "pub fn compute_main(x: i64) -> i64 { x + 1 }\n",
    )
    .expect("write compute.rs fixture");

    // Alias `aliased` — exercises the explicit `as <ident>` form
    // (`include_relon!(\"src/whatever.relon\" as aliased)`), where the
    // filename stem and the binding alias intentionally differ.
    fs::write(
        bindings_dir.join("aliased.rs"),
        "pub fn aliased_double(x: i64) -> i64 { x * 2 }\n",
    )
    .expect("write aliased.rs fixture");

    // Only rerun when this script itself changes; the fixtures are
    // self-contained and have no external inputs.
    println!("cargo:rerun-if-changed=build.rs");
}
