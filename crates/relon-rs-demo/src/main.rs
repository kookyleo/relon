//! End-to-end demo for the build-time Relon -> object-file pipeline.
//!
//! `build.rs` compiled `src/foo.relon` (`#main(Int n) -> Int : n * 2`)
//! at build time. The `include_relon!` macro splices the generated
//! `pub mod foo { pub fn main(&SandboxState, i64) -> i64 { ... } }`
//! shim into this file. Calling `foo::main(&state, 42)` dispatches
//! straight to the AOT-compiled body via a single `extern "C"` call.

relon_rs_macro::include_relon!("src/foo.relon");

fn main() {
    let state = relon_rs_shims::SandboxState::default();
    let n: i64 = 42;
    let result = foo::main(&state, n);
    println!("foo::main({n}) = {result}");
    assert_eq!(
        result,
        n * 2,
        "AOT-compiled Relon body must agree with `n * 2`"
    );
}
