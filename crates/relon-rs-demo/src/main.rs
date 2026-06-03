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
//! - `src/qux.relon` — Float + Int → Float (`#main(Float x, Int n) ->
//!   Float : x * 2.5 + n / 2.0`). Exercises the widened native
//!   signature surface: an `f64` parameter packed into / decoded out
//!   of the arena alongside a mixed-type (`Int`) param.
//! - `src/quux.relon` — Int → List<Int> (`#main(Int n) -> List<Int> :
//!   [n, n + 1, 7]`). Exercises a `Vec<i64>` return marshalled out of
//!   the arena's `[len][i64…]` tail record.
//!
//! The `include_relon!` macro stitches each source's generated binding
//! into this file, exposing `foo::main(&state, …)`, `bar::main(&state,
//! …)`, `baz::main(&state, …)`, `qux::main(&state, …)`, and
//! `quux::main(&state, …)` as plain Rust functions.

relon_rs_macro::include_relon!("src/foo.relon");
relon_rs_macro::include_relon!("src/bar.relon");
relon_rs_macro::include_relon!("src/baz.relon");
relon_rs_macro::include_relon!("src/qux.relon");
relon_rs_macro::include_relon!("src/quux.relon");
relon_rs_macro::include_relon!("src/secret.relon");

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

    // qux — buffer-protocol Float + Int → Float. The `f64` arg rides
    // the arena's 8-byte inline slot; the result decodes back through
    // `RetValue::Float`.
    let qux_x = 4.0_f64;
    let qux_n: i64 = 3;
    let qux_r = qux::main(&state, qux_x, qux_n);
    println!("qux::main({qux_x}, {qux_n}) = {qux_r}");
    let qux_want = qux_x * 2.5 + qux_n as f64 / 2.0;
    assert_eq!(qux_r, qux_want, "buffer-protocol Float mismatch");

    // quux — buffer-protocol Int → List<Int>. The returned `Vec<i64>`
    // is copied out of the arena's `[len][i64…]` tail record before
    // the per-call buffer recycles.
    let quux_n: i64 = 10;
    let quux_r = quux::main(&state, quux_n);
    println!("quux::main({quux_n}) = {quux_r:?}");
    assert_eq!(
        quux_r,
        vec![quux_n, quux_n + 1, 7],
        "buffer-protocol List<Int> return mismatch"
    );

    // secret — buffer-protocol Int → Int gated on `reads_clock`. The
    // `.relon` body calls the host `#native` fn `clock_add` (linked +
    // inlined into the `.o` at build time). The `Op::CheckCap` gate the
    // emitter baked in is enforced at runtime against the caps mask the
    // `SandboxState` carries. The binding returns `Result` because the
    // source is gated.
    let secret_x: i64 = 42;

    // GRANT: a SandboxState granting `reads_clock` authorises the gated
    // call; the inlined host body runs and returns the value.
    let granted = relon_rs_shims::SandboxState::new().grant(relon_rs_shims::CapabilityBit::ReadsClock);
    let secret_ok = secret::main(&granted, secret_x);
    println!("secret::main(grant reads_clock, {secret_x}) = {secret_ok:?}");
    assert_eq!(
        secret_ok.expect("granted gate must run the body"),
        secret_x.wrapping_add(1_700_000_000),
        "granted #native call must return the inlined host fn's value"
    );

    // DENY: a zero-trust SandboxState withholds `reads_clock`; the gate
    // traps and the marshaller lifts a typed CapabilityDenied — no
    // crash, no SIGILL.
    let denied_state = relon_rs_shims::SandboxState::new();
    let secret_err = secret::main(&denied_state, secret_x);
    println!("secret::main(deny, {secret_x}) = {secret_err:?}");
    assert!(
        matches!(
            secret_err,
            Err(relon_rs_shims::BufferEntryError::CapabilityDenied)
        ),
        "ungranted #native call must return a typed CapabilityDenied, got {secret_err:?}"
    );

    println!(
        "\nrelon-rs demo: five plain call shapes returned the expected value; \
         the gated #native call granted -> {} and denied -> CapabilityDenied.",
        secret_x.wrapping_add(1_700_000_000)
    );
}
