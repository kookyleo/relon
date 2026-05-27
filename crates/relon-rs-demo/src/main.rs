//! End-to-end demo for the build-time Relon -> object-file pipeline.
//!
//! `build.rs` drove `relon-rs-build::Compiler` over three sources at
//! build time:
//!
//! - `src/foo.relon` — Int-only `#main(Int) -> Int : n * 2`. Lowers
//!   onto the Phase 1 dispatch-boundary fast path (`extern "C" fn(i64)
//!   -> i64`). No arena, no shim dependency.
//! - `src/bar.relon` — String → Int (`#main(String) -> Int : length(s)`).
//!   Forces the buffer-protocol entry: `&str` gets packed into the
//!   arena, the JIT body reads `ReadStringLen`, the return record's
//!   single Int slot rides back through the marshaller.
//! - `src/baz.relon` — String → Bool (`#main(String) -> Bool :
//!   s.contains("x")`). Exercises both the buffer-protocol entry and
//!   the `relon_llvm_str_contains_arena` host shim re-exported from
//!   `relon-rs-shims`.
//!
//! The `include_relon!` macro stitches each source's generated binding
//! into this file, exposing `foo::main(&state, …)`, `bar::main(&state,
//! …)`, and `baz::main(&state, …)` as plain Rust functions.

relon_rs_macro::include_relon!("src/foo.relon");
relon_rs_macro::include_relon!("src/bar.relon");
relon_rs_macro::include_relon!("src/baz.relon");

fn main() {
    let state = relon_rs_shims::SandboxState::default();

    // foo — fast-path Int entry. `n * 2` lowered to a typed
    // `extern "C" fn(i64) -> i64` invocation.
    let n: i64 = 42;
    let foo_r = foo::main(&state, n);
    println!("foo::main({n}) = {foo_r}");
    assert_eq!(foo_r, n * 2, "fast-path Int entry mismatch");

    // bar — buffer-protocol String → Int. `length(s)` lowered to
    // `Op::ReadStringLen` inside the JIT body.
    let bar_input = "hello, relon!";
    let bar_r = bar::main(&state, bar_input);
    println!("bar::main({bar_input:?}) = {bar_r}");
    assert_eq!(
        bar_r,
        bar_input.len() as i64,
        "buffer-protocol String → Int mismatch"
    );

    // baz — buffer-protocol String → Bool. Routes through the
    // `relon_llvm_str_contains_arena` host shim.
    let baz_match = "axb";
    let baz_miss = "qqq";
    let baz_r_match = baz::main(&state, baz_match);
    let baz_r_miss = baz::main(&state, baz_miss);
    println!("baz::main({baz_match:?}) = {baz_r_match}");
    println!("baz::main({baz_miss:?}) = {baz_r_miss}");
    assert!(
        baz_r_match,
        "baz must report contains('x') for {baz_match:?}"
    );
    assert!(
        !baz_r_miss,
        "baz must report !contains('x') for {baz_miss:?}"
    );

    println!("\nrelon-rs Phase 2 demo: all three call shapes returned the expected value.");
}
