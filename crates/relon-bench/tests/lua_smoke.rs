//! v6-λ-1 (2026-05-19): mlua + LuaJIT smoke tests. These verify the
//! `mlua = { features = ["luajit", "vendored", "macros"] }` dev-dep is
//! wired correctly before the λ-2 paired workload phase starts writing
//! Lua-side benches.
//!
//! Two gated tests run on every `cargo test -p relon-bench` round:
//!
//! - `lua_one_plus_one_is_two` — minimal LuaJIT roundtrip.
//! - `lua_sum_loop_returns_expected` — a 100-iter sum loop returns 4950.
//!
//! One `#[ignore]` test (`lua_boundary_cost_in_ballpark`) measures the
//! mlua→LuaJIT call overhead and asserts it falls in the 50-1000 ns
//! envelope. Skipped by default because it's wall-clock-sensitive and
//! would flake in noisy CI.
//!
//! These are NOT benches — they only exist to fail loud if the LuaJIT
//! integration breaks (build flag flip, mlua version bump that ejects
//! `luajit`, etc.).

use mlua::{Lua, Value};

#[test]
fn lua_one_plus_one_is_two() {
    let lua = Lua::new();
    let result: Value = lua
        .load("return 1 + 1")
        .eval()
        .expect("eval `return 1 + 1` must succeed");
    match result {
        Value::Integer(2) => {}
        other => panic!("expected Value::Integer(2), got {other:?}"),
    }
}

#[test]
fn lua_sum_loop_returns_expected() {
    let lua = Lua::new();
    // sum 0..99 = 4950 (LuaJIT uses 1-based iteration semantics for `for`
    // but we explicitly run `0..99` via `i = 0, 99, 1` so the sum is the
    // same as `for i in 0..100 { acc += i }` in Rust).
    let result: Value = lua
        .load(
            r#"
            local acc = 0
            for i = 0, 99 do
                acc = acc + i
            end
            return acc
            "#,
        )
        .eval()
        .expect("eval sum loop must succeed");
    match result {
        Value::Integer(4950) => {}
        other => panic!("expected Value::Integer(4950), got {other:?}"),
    }
}

#[test]
#[ignore = "wall-clock sensitive; run with --ignored on a quiescent box"]
fn lua_boundary_cost_in_ballpark() {
    use std::time::Instant;

    let lua = Lua::new();
    // The cheapest possible Lua function — returns a constant. Used to
    // approximate the mlua→Lua boundary cost; subtract this from any
    // λ-2 workload number to isolate "what Lua actually does".
    let noop: mlua::Function = lua
        .load("return function() return 42 end")
        .eval()
        .expect("create noop fn");
    // Warm up the call site.
    for _ in 0..10_000u32 {
        let _: i64 = noop.call(()).expect("call noop");
    }
    let n: u32 = 1_000_000;
    let start = Instant::now();
    for _ in 0..n {
        let _: i64 = noop.call(()).expect("call noop");
    }
    let elapsed = start.elapsed();
    let ns_per_call = elapsed.as_nanos() as f64 / f64::from(n);
    eprintln!("lua_boundary_cost_in_ballpark: {n} calls in {elapsed:?} → {ns_per_call:.1} ns/call");
    // The mlua→Lua boundary on x86_64 lands around 50-200 ns/call when
    // the host machine is quiescent. We widen the upper bound to 1000 ns
    // here because this test is `#[ignore]`d for general use and the
    // assertion only fires when someone explicitly runs `--ignored`.
    assert!(
        (20.0..=1000.0).contains(&ns_per_call),
        "expected 20-1000 ns/call (LuaJIT boundary baseline); got {ns_per_call:.1} ns/call"
    );
}
