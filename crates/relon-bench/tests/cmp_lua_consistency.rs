//! v6-lambda-2 consistency check: parse + evaluate each W's Relon source
//! and compare against the analytic expected value, plus exercise the Lua
//! side under mlua. This is the integration test that catches Relon source
//! / Lua source / expected-value drift BEFORE the criterion bench loop
//! attempts to run (the bench panics inline on consistency mismatch, but
//! waiting 4+ minutes for cargo bench to compile in release mode just to
//! discover a typo is slow).
//!
//! Each test mirrors one workload's setup-time consistency assert from
//! `benches/cmp_lua.rs` and runs the same Relon + Lua source pair.

use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::Value;
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

fn build_tree_walker(src: &str) -> (TreeWalkEvaluator, Arc<Scope>) {
    let node = parse_document(src)
        .unwrap_or_else(|e| panic!("parse failed for source:\n{src}\nerror: {e:?}"));
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    (
        TreeWalkEvaluator::new(Arc::new(ctx)),
        Arc::new(Scope::default()),
    )
}

fn lua_fn(lua: &mlua::Lua, src: &str) -> mlua::Function {
    lua.load(src)
        .eval::<mlua::Function>()
        .unwrap_or_else(|e| panic!("Lua fn compile failed:\n{src}\nerror: {e}"))
}

fn args_n(n: i64) -> HashMap<String, Value> {
    let mut m = HashMap::with_capacity(1);
    m.insert("n".to_string(), Value::Int(n));
    m
}

fn relon_int_result(w: &str, v: Value) -> i64 {
    match v {
        Value::Int(n) => n,
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Int(n)) => *n,
            other => panic!("{w}: dict.result not Int: {other:?}"),
        },
        other => panic!("{w}: not Int or Dict: {other:?}"),
    }
}

fn relon_float_result(w: &str, v: Value) -> f64 {
    match v {
        Value::Float(f) => f.into_inner(),
        Value::Dict(d) => match d.map.get("result") {
            Some(Value::Float(f)) => f.into_inner(),
            other => panic!("{w}: dict.result not Float: {other:?}"),
        },
        other => panic!("{w}: not Float or Dict: {other:?}"),
    }
}

fn run_pair(w: &str, relon_src: &str, lua_src: &str, n: i64, expected: i64) {
    let (walker, scope) = build_tree_walker(relon_src);
    let relon_v = relon_int_result(w, walker.run_main(&scope, args_n(n)).unwrap());
    assert_eq!(
        relon_v, expected,
        "{w}: Relon got {relon_v}, expected {expected}"
    );

    let lua = mlua::Lua::new();
    let f = lua_fn(&lua, lua_src);
    let lua_v: i64 = f.call(()).unwrap();
    assert_eq!(lua_v, expected, "{w}: Lua got {lua_v}, expected {expected}");
}

/// Float variant of `run_pair`: the tree-walker and Lua results are
/// compared against `expected` within an absolute tolerance (Float
/// reduce ordering differs by ~1 ULP per iter between the tree-walker
/// and LuaJIT, so an exact-equal check is wrong here — see the W20 /
/// W28 doc-comments in `benches/cmp_lua.rs`).
fn run_pair_float(w: &str, relon_src: &str, lua_src: &str, n: i64, expected: f64, tol: f64) {
    let (walker, scope) = build_tree_walker(relon_src);
    let relon_v = relon_float_result(w, walker.run_main(&scope, args_n(n)).unwrap());
    assert!(
        (relon_v - expected).abs() <= tol,
        "{w}: Relon got {relon_v}, expected {expected} (tol {tol})"
    );

    let lua = mlua::Lua::new();
    let f = lua_fn(&lua, lua_src);
    let lua_v: f64 = f.call(()).unwrap();
    assert!(
        (lua_v - expected).abs() <= tol,
        "{w}: Lua got {lua_v}, expected {expected} (tol {tol})"
    );
}

const W1_N: i64 = 10_000;
const TREE_WALK_N: i64 = 10_000;
const STRING_CONCAT_N: i64 = 2_000;
const FIB_N: i64 = 22;
const CONFIG_QUERIES_N: i64 = 1_000;

#[test]
fn w1_int_sum() {
    let r = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
    let l = format!(
        "return function() local acc = 0; for i = 0, {} - 1 do acc = acc + i end; return acc end",
        W1_N
    );
    let expected = W1_N * (W1_N - 1) / 2;
    run_pair("W1", r, &l, W1_N, expected);
}

#[test]
fn w2_f64_dot() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Int\n\
             list.sum(range(n).map((i) => (i + 1) * (i + 2)))";
    let n: i64 = 1_000;
    let l = format!(
        "return function() local n = {n}; local xs = {{}}; local ys = {{}};\n\
         for i = 1, n do xs[i] = i; ys[i] = i + 1 end;\n\
         local sum = 0; for i = 1, n do sum = sum + xs[i] * ys[i] end; return sum end"
    );
    let mut expected: i64 = 0;
    for i in 0..n {
        expected += (i + 1) * (i + 2);
    }
    run_pair("W2", r, &l, n, expected);
}

#[test]
fn w3_string_concat() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> String\n\
             range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
    let n = STRING_CONCAT_N;
    let l = format!(
        "return function() local n = {n}; local s = \"\"; for i = 1, n do s = s .. \"a\" end; return #s end"
    );
    let (walker, scope) = build_tree_walker(r);
    let relon_v = match walker.run_main(&scope, args_n(n)).unwrap() {
        Value::String(s) => s.len() as i64,
        other => panic!("W3 Relon non-string: {other:?}"),
    };
    let lua = mlua::Lua::new();
    let f = lua_fn(&lua, &l);
    let lua_v: i64 = f.call(()).unwrap();
    assert_eq!(relon_v, n);
    assert_eq!(lua_v, n);
}

#[test]
fn w4_string_contains() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Int\n\
             range(n).map((i) => \"axb\").filter((s) => s.contains(\"x\")).len()";
    let n = TREE_WALK_N;
    let l = format!(
        "return function() local n = {n}; local c = 0;\n\
         for i = 1, n do if string.find(\"axb\", \"x\", 1, true) then c = c + 1 end end;\n\
         return c end"
    );
    run_pair("W4", r, &l, n, n);
}

#[test]
fn w5_dict_str_key() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Dict\n\
             {\n\
               #internal\n\
               d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
               #internal\n\
               keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
               result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
             }";
    let n = TREE_WALK_N;
    let l = format!(
        "return function()\n\
         local d = {{a=1,b=2,c=3,d=4,e=5,f=6,g=7,h=8,i=9,j=10}};\n\
         local keys = {{\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\",\"h\",\"i\",\"j\"}};\n\
         local n = {n}; local sum = 0;\n\
         for i = 1, n do local k = keys[((i - 1) % 10) + 1]; sum = sum + d[k] end;\n\
         return sum end"
    );
    let full = n / 10;
    let expected = full * 55;
    run_pair("W5", r, &l, n, expected);
}

#[test]
fn w6_dict_num_key() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Int\n\
             list.sum(range(n).map((i) => i + 1))";
    let n = TREE_WALK_N;
    let l = format!(
        "return function() local n = {n}; local arr = {{}};\n\
         for i = 1, n do arr[i] = i end;\n\
         local sum = 0; for i = 1, n do sum = sum + arr[i] end; return sum end"
    );
    let expected = n * (n + 1) / 2;
    run_pair("W6", r, &l, n, expected);
}

#[test]
fn w7_fib() {
    // The tree-walker recurses 17.7k times for fib(22), each frame
    // cloning a Scope (>= 64B) + step counter + path_cache lookups.
    // Default test thread stack (2 MB) overflows; spawn an explicit
    // 16 MB thread so the test passes regardless of `RUST_MIN_STACK`.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let r = "#main(Int n) -> Dict\n\
                     {\n\
                       #internal\n\
                       fib: (Int k) -> Int => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                       result: fib(n)\n\
                     }";
            let n = FIB_N;
            let l = format!(
                "return function() local function fib(k) if k < 2 then return k end; return fib(k-1) + fib(k-2) end; return fib({n}) end"
            );
            fn fib(k: i64) -> i64 {
                if k < 2 {
                    k
                } else {
                    fib(k - 1) + fib(k - 2)
                }
            }
            let expected = fib(n);
            run_pair("W7", r, &l, n, expected);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn w8_poly_callsite() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Dict\n\
             {\n\
               #internal\n\
               dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4,\n\
               result: list.sum(range(n).map((i) => dispatch(i % 4)))\n\
             }";
    let n = TREE_WALK_N;
    let l = format!(
        "return function() local function dispatch(t) if t == 0 then return 1\n\
         elseif t == 1 then return 2 elseif t == 2 then return 3 else return 4 end end;\n\
         local sum = 0; for i = 0, {n} - 1 do sum = sum + dispatch(i % 4) end; return sum end"
    );
    let full = n / 4;
    let expected = full * 10;
    run_pair("W8", r, &l, n, expected);
}

#[test]
fn w9_nested_matrix() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Dict\n\
             {\n\
               #internal\n\
               rows: range(n).map((i) => range(n).map((j) => i * n + j)),\n\
               result: range(n).reduce(0, (acc, j) =>\n\
                 acc + range(n).reduce(0, (inner, i) => inner + rows[i][j]))\n\
             }";
    let n: i64 = 32;
    let l = format!(
        "return function() local n = {n}; local m = {{}};\n\
         for i = 1, n do m[i] = {{}}; for j = 1, n do m[i][j] = (i-1)*n + (j-1) end end;\n\
         local s = 0; for j = 1, n do for i = 1, n do s = s + m[i][j] end end; return s end"
    );
    let mut expected: i64 = 0;
    for i in 0..n {
        for j in 0..n {
            expected += i * n + j;
        }
    }
    run_pair("W9", r, &l, n, expected);
}

/// W13 — deep dict access. Models a config-tree walk that touches two
/// 5-level-deep nested dict literal leaves per iter. Per HONESTY_POLICY:
/// * Source path: production source is byte-identical to bench helper.
/// * Algorithm: O(n) reduce, two dict-chain reads per iter; constants
///   in dict literal NOT folded into the kernel (analyzer treats the
///   dict-body `#internal cfg` as a runtime binding).
/// * I/O shape: `#main(Int n) -> Dict` with `result` field; Lua equivalent
///   sums the same constant pair per iter, returns the same `i64`.
#[test]
fn w13_deep_dict_access() {
    let r = "#unstrict\n\
             #main(Int n) -> Dict\n\
             {\n\
               #internal\n\
               cfg: { db: { pool: { connections: { max: 100, timeout: { ms: 5000 } } } } },\n\
               result: range(n).reduce(0, (acc, i) =>\n\
                 acc + cfg.db.pool.connections.max + cfg.db.pool.connections.timeout.ms)\n\
             }";
    let n: i64 = 1_000;
    let l = format!(
        "return function() local n = {n};\n\
         local cfg = {{ db = {{ pool = {{ connections = {{ max = 100, timeout = {{ ms = 5000 }} }} }} }} }};\n\
         local acc = 0;\n\
         for i = 0, n - 1 do\n\
           acc = acc + cfg.db.pool.connections.max + cfg.db.pool.connections.timeout.ms\n\
         end;\n\
         return acc end"
    );
    let expected = n * (100 + 5000);
    run_pair("W13", r, &l, n, expected);
}

/// W14 — schema-validate. Per-iter boolean range checks (both
/// trivially-true), emulating Relon's `#expect ...` schema gate
/// surface in a tight reduce loop. Per HONESTY_POLICY:
/// * Source path: byte-identical to bench helper.
/// * Algorithm: O(n) reduce, two boolean range checks per iter (each
///   compiles to a comparison chain — no closed form in Relon source).
/// * I/O shape: `#main(Int n) -> Int`; Lua equivalent runs the same
///   ternary chain.
#[test]
fn w14_schema_validate() {
    let r = "#unstrict\n\
             #main(Int n) -> Int\n\
             range(n).reduce(0, (acc, i) =>\n\
               acc\n\
                 + ((i % 10) >= 0 && (i % 10) < 10 ? 1 : 0)\n\
                 + ((i / 10) >= 0 && (i / 10) < 1000 ? 1 : 0))";
    let n: i64 = 1_000;
    let l = format!(
        "return function() local n = {n}; local acc = 0;\n\
         for i = 0, n - 1 do\n\
           local role = i % 10\n\
           local region = math.floor(i / 10)\n\
           acc = acc\n\
             + ((role >= 0 and role < 10) and 1 or 0)\n\
             + ((region >= 0 and region < 1000) and 1 or 0)\n\
         end;\n\
         return acc end"
    );
    let expected = n * 2;
    run_pair("W14", r, &l, n, expected);
}

/// W15 — conditional field. Per-iter ternary picks one of two analytic
/// expressions, the canonical `?:` declarative-DSL render pattern.
/// Per HONESTY_POLICY:
/// * Source path: byte-identical to bench helper.
/// * Algorithm: O(n) reduce, branch + multiply per iter.
/// * I/O shape: `#main(Int n) -> Int`; Lua equivalent picks the same
///   branch via Lua's `if .. then .. else .. end` form.
#[test]
fn w15_conditional_field() {
    let r = "#unstrict\n\
             #main(Int n) -> Int\n\
             range(n).reduce(0, (acc, i) =>\n\
               acc + (i % 2 == 0 ? i * 2 : i * 3))";
    let n: i64 = 1_000;
    let l = format!(
        "return function() local n = {n}; local acc = 0;\n\
         for i = 0, n - 1 do\n\
           if (i % 2) == 0 then acc = acc + i * 2 else acc = acc + i * 3 end\n\
         end;\n\
         return acc end"
    );
    let mut expected: i64 = 0;
    for i in 0..n {
        expected += if i % 2 == 0 { i * 2 } else { i * 3 };
    }
    run_pair("W15", r, &l, n, expected);
}

#[test]
fn w10_config_eval() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Dict\n\
             {\n\
               #internal\n\
               allow: (i) =>\n\
                 (i % 3 == 0 || i % 3 == 1) &&\n\
                 (i % 4 == 0 || i % 4 == 1) &&\n\
                 (i % 24 >= 8 && i % 24 < 18) ? 1 : 0,\n\
               result: list.sum(range(n).map(allow))\n\
             }";
    let n = CONFIG_QUERIES_N;
    let l = format!(
        "return function() local n = {n}; local c = 0;\n\
         for i = 0, n - 1 do\n\
           local ri = i % 3; local re = i % 4; local h = i % 24;\n\
           if (ri == 0 or ri == 1) and (re == 0 or re == 1) and (h >= 8 and h < 18) then\n\
             c = c + 1 end end;\n\
         return c end"
    );
    let mut expected: i64 = 0;
    for i in 0..n {
        let ri = i % 3;
        let re = i % 4;
        let h = i % 24;
        if (ri == 0 || ri == 1) && (re == 0 || re == 1) && (8..18).contains(&h) {
            expected += 1;
        }
    }
    run_pair("W10", r, &l, n, expected);
}

// =====================================================================
// =====  W16-W30 — Tier 2-4 panel workloads  ==========================
// =====================================================================
//
// Mirrors the per-workload source/expected helpers in
// `benches/cmp_lua.rs` (`wN_relon_src` / `wN_lua_src` / `wN_expected`).
// The bench file's helpers are not importable (they live inside the
// bench crate's `benches/` target), so the minimal source strings are
// replicated here as test constants. Skipped: W11 / W22 / W29 (absent
// or blocked in the bench). W12 is also a pure pass-through helper with
// no `#main` loop body in the panel and is left to the bench's inline
// assert.

const W16_N: i64 = 1_000;
const W17_N: i64 = 100;
const W18_N: i64 = 10_000;
const W19_N: i64 = 16;
const W20_N: i64 = 1_000;
const W20_FLOAT_TOL: f64 = 1.0e-6;
const W21_N: i64 = 10_000;
const W23_N: i64 = 10_000;
const W24_N: i64 = 10_000;
const W25_N: i64 = 10_000;
const W26_N: i64 = 1_000;
const W27_N: i64 = 10_000;
const W28_N: i64 = 10_000;
const W28_FLOAT_TOL: f64 = 1.0e-6;
const W30_N: i64 = 10_000;

/// W16 — sum-via-partition quicksort recurrence. Recurses; runs on an
/// explicit large-stack thread like W7.
#[test]
fn w16_quicksort_sum() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let r = "#unstrict\n\
                     #import list from \"std/list\"\n\
                     #main(Int n) -> Int\n\
                     sum_qs(arr)\n\
                     where {\n\
                       arr: range(n).map((i) => (i * 1103515245 + 12345) % 2048),\n\
                       sum_qs(xs): _len(xs) <= 1 ? (_len(xs) == 0 ? 0 : xs[0]) : (\n\
                         sum_qs(_list_filter(xs, (x) => x < xs[0]))\n\
                         + list.sum(_list_filter(xs, (x) => x == xs[0]))\n\
                         + sum_qs(_list_filter(xs, (x) => x > xs[0]))\n\
                       )\n\
                     }";
            let n = W16_N;
            let l = format!(
                r#"return function()
                    local n = {n}
                    local arr = {{}}
                    for i = 0, n - 1 do
                        arr[i + 1] = (i * 1103515245 + 12345) % 2048
                    end
                    local function sum_qs(xs)
                        local len = #xs
                        if len == 0 then return 0 end
                        if len == 1 then return xs[1] end
                        local p = xs[1]
                        local lt, eq_sum, gt = {{}}, 0, {{}}
                        for i = 1, len do
                            local v = xs[i]
                            if v < p then lt[#lt + 1] = v
                            elseif v > p then gt[#gt + 1] = v
                            else eq_sum = eq_sum + v end
                        end
                        return sum_qs(lt) + eq_sum + sum_qs(gt)
                    end
                    return sum_qs(arr)
                end"#
            );
            let mut expected: i64 = 0;
            for i in 0..n {
                expected += (i.wrapping_mul(1103515245).wrapping_add(12345)) % 2048;
            }
            run_pair("W16", r, &l, n, expected);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// W17 — binary search (recursive bisection over `range(n)`). Recurses;
/// large-stack thread for parity with the bench's per-frame cost.
#[test]
fn w17_binary_search() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let r = "#unstrict\n\
                     #main(Int n) -> Int\n\
                     range(n).reduce(0, (acc, i) => acc + bs(0, n, (i * 31) % n))\n\
                     where {\n\
                       bs(lo, hi, t): hi - lo <= 1 ? lo : (\n\
                         (lo + hi) / 2 <= t\n\
                           ? bs((lo + hi) / 2, hi, t)\n\
                           : bs(lo, (lo + hi) / 2, t)\n\
                       )\n\
                     }";
            let n = W17_N;
            let l = format!(
                r#"return function()
                    local n = {n}
                    local function bs(lo, hi, t)
                        if hi - lo <= 1 then return lo end
                        local mid = math.floor((lo + hi) / 2)
                        if mid <= t then return bs(mid, hi, t)
                        else return bs(lo, mid, t) end
                    end
                    local acc = 0
                    for i = 0, n - 1 do
                        acc = acc + bs(0, n, (i * 31) % n)
                    end
                    return acc
                end"#
            );
            let mut expected: i64 = 0;
            for i in 0..n {
                expected += (i.wrapping_mul(31)) % n;
            }
            run_pair("W17", r, &l, n, expected);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// W18 — prime count via trial-division (recursive divisor probe).
#[test]
fn w18_prime_count() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let r = "#unstrict\n\
                     #main(Int n) -> Int\n\
                     _len(_list_filter(range(2, n), (k) => is_prime(k, 2)))\n\
                     where {\n\
                       is_prime(k, d): d * d > k ? true : (k % d == 0 ? false : is_prime(k, d + 1))\n\
                     }";
            let n = W18_N;
            let l = format!(
                r#"return function()
                    local n = {n}
                    local function is_prime(k, d)
                        if d * d > k then return true end
                        if k % d == 0 then return false end
                        return is_prime(k, d + 1)
                    end
                    local count = 0
                    for k = 2, n - 1 do
                        if is_prime(k, 2) then count = count + 1 end
                    end
                    return count
                end"#
            );
            fn is_prime(k: i64) -> bool {
                let mut d: i64 = 2;
                while d.saturating_mul(d) <= k {
                    if k % d == 0 {
                        return false;
                    }
                    d += 1;
                }
                true
            }
            let mut expected: i64 = 0;
            let mut k: i64 = 2;
            while k < n {
                if is_prime(k) {
                    expected += 1;
                }
                k += 1;
            }
            run_pair("W18", r, &l, n, expected);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// W19 — matrix multiply (O(n^3) triple-nested reduce).
#[test]
fn w19_matrix_multiply() {
    let r = "#unstrict\n\
             #main(Int n) -> Int\n\
             c.reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))\n\
             where {\n\
               size: n,\n\
               a: range(size).map((i) => range(size).map((j) => (i * size + j) % 100)),\n\
               b: range(size).map((i) => range(size).map((j) => (i + j) % 100)),\n\
               c: range(size).map((i) => range(size).map((j) => range(size).reduce(0, (acc, k) => acc + a[i][k] * b[k][j])))\n\
             }";
    let n = W19_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local size = n
            local a, b = {{}}, {{}}
            for i = 1, size do
                a[i], b[i] = {{}}, {{}}
                for j = 1, size do
                    a[i][j] = ((i - 1) * size + (j - 1)) % 100
                    b[i][j] = ((i - 1) + (j - 1)) % 100
                end
            end
            local result = 0
            for i = 1, size do
                for j = 1, size do
                    local s = 0
                    for k = 1, size do
                        s = s + a[i][k] * b[k][j]
                    end
                    result = result + s
                end
            end
            return result
        end"#
    );
    let size: i64 = n;
    let mut expected: i64 = 0;
    let mut i: i64 = 0;
    while i < size {
        let mut j: i64 = 0;
        while j < size {
            let mut s: i64 = 0;
            let mut k: i64 = 0;
            while k < size {
                let aik = (i.wrapping_mul(size).wrapping_add(k)) % 100;
                let bkj = (k.wrapping_add(j)) % 100;
                s = s.wrapping_add(aik.wrapping_mul(bkj));
                k += 1;
            }
            expected = expected.wrapping_add(s);
            j += 1;
        }
        i += 1;
    }
    run_pair("W19", r, &l, n, expected);
}

/// W20 — softened 4-body 1D Verlet integration; Float return checked
/// within absolute tolerance.
#[test]
fn w20_n_body_softened() {
    let r = "#unstrict\n\
             #main(Int n) -> Float\n\
             final_state[0] * 1.0 + final_state[1] * 2.0 + final_state[2] * 3.0 + final_state[3] * 4.0\n\
               + final_state[4] * 5.0 + final_state[5] * 6.0 + final_state[6] * 7.0 + final_state[7] * 8.0\n\
             where {\n\
               dt: 0.01,\n\
               soft: 0.1,\n\
               m0: 1.0, m1: 2.0, m2: 0.5, m3: 3.0,\n\
               init: [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2],\n\
               pair_force(s, i, j, mj):\n\
                 i == j ? 0.0 :\n\
                   (s[j] - s[i]) * mj * (1.0 / (((s[j] - s[i]) * (s[j] - s[i]) + soft) * ((s[j] - s[i]) * (s[j] - s[i]) + soft))),\n\
               accel(s, i): pair_force(s, i, 0, m0) + pair_force(s, i, 1, m1) + pair_force(s, i, 2, m2) + pair_force(s, i, 3, m3),\n\
               step(s): [\n\
                 s[0] + s[4] * dt,\n\
                 s[1] + s[5] * dt,\n\
                 s[2] + s[6] * dt,\n\
                 s[3] + s[7] * dt,\n\
                 s[4] + accel(s, 0) * dt,\n\
                 s[5] + accel(s, 1) * dt,\n\
                 s[6] + accel(s, 2) * dt,\n\
                 s[7] + accel(s, 3) * dt\n\
               ],\n\
               final_state: range(n).reduce(init, (s, _step) => step(s))\n\
             }";
    let n = W20_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local dt = 0.01
            local soft = 0.1
            local m = {{1.0, 2.0, 0.5, 3.0}}
            local s = {{0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2}}
            local function pair_force(s, i, j, mj)
                if i == j then return 0.0 end
                local dx = s[j + 1] - s[i + 1]
                local r2 = dx * dx + soft
                return dx * mj * (1.0 / (r2 * r2))
            end
            local function accel(s, i)
                return pair_force(s, i, 0, m[1]) + pair_force(s, i, 1, m[2])
                     + pair_force(s, i, 2, m[3]) + pair_force(s, i, 3, m[4])
            end
            for _ = 1, n do
                local ns = {{
                    s[1] + s[5] * dt,
                    s[2] + s[6] * dt,
                    s[3] + s[7] * dt,
                    s[4] + s[8] * dt,
                    s[5] + accel(s, 0) * dt,
                    s[6] + accel(s, 1) * dt,
                    s[7] + accel(s, 2) * dt,
                    s[8] + accel(s, 3) * dt,
                }}
                s = ns
            end
            return s[1] * 1.0 + s[2] * 2.0 + s[3] * 3.0 + s[4] * 4.0
                 + s[5] * 5.0 + s[6] * 6.0 + s[7] * 7.0 + s[8] * 8.0
        end"#
    );
    let dt: f64 = 0.01;
    let soft: f64 = 0.1;
    let m: [f64; 4] = [1.0, 2.0, 0.5, 3.0];
    let mut s: [f64; 8] = [0.0, 1.0, 2.5, 4.0, 0.1, 0.0, 0.0, 0.2];
    let mut step = 0i64;
    while step < n {
        let mut a: [f64; 4] = [0.0; 4];
        for i in 0..4 {
            let mut ai = 0.0;
            for j in 0..4 {
                if i == j {
                    continue;
                }
                let dx = s[j] - s[i];
                let r2 = dx * dx + soft;
                ai += dx * m[j] * (1.0 / (r2 * r2));
            }
            a[i] = ai;
        }
        let mut ns: [f64; 8] = [0.0; 8];
        for i in 0..4 {
            ns[i] = s[i] + s[4 + i] * dt;
            ns[4 + i] = s[4 + i] + a[i] * dt;
        }
        s = ns;
        step += 1;
    }
    let expected = s[0] * 1.0
        + s[1] * 2.0
        + s[2] * 3.0
        + s[3] * 4.0
        + s[4] * 5.0
        + s[5] * 6.0
        + s[6] * 7.0
        + s[7] * 8.0;
    run_pair_float("W20", r, &l, n, expected, W20_FLOAT_TOL);
}

/// W21 — match dispatch over brand-tagged values. Dict-bodied `result`.
#[test]
fn w21_match_dispatch() {
    let r = "#unstrict\n\
             #main(Int n) -> Dict\n\
             {\n\
               #schema Image { name: String, url: String },\n\
               #schema Text { name: String, content: String },\n\
               items: [\n\
                 #brand Image { name: \"img\", url: \"http://a.png\" },\n\
                 #brand Text { name: \"txt\", content: \"hello\" }\n\
               ],\n\
               classify(it): it match {\n\
                 Image: 1,\n\
                 Text: 2,\n\
                 _: 0\n\
               },\n\
               result: range(n).reduce(0, (acc, i) => acc + classify(items[i % 2]))\n\
             }";
    let n = W21_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local items = {{
                {{ __type = "Image", name = "img", url = "http://a.png" }},
                {{ __type = "Text", name = "txt", content = "hello" }},
            }}
            local function classify(it)
                if it.__type == "Image" then
                    return 1
                elseif it.__type == "Text" then
                    return 2
                else
                    return 0
                end
            end
            local acc = 0
            for i = 0, n - 1 do
                acc = acc + classify(items[(i % 2) + 1])
            end
            return acc
        end"#
    );
    let expected = n * 3 / 2;
    run_pair("W21", r, &l, n, expected);
}

/// W23 — dict spread copy per iter.
#[test]
fn w23_dict_spread() {
    let r = "#unstrict\n\
             #main(Int n) -> Int\n\
             range(n).reduce(0, (acc, _) =>\n\
               acc + _len({ ...base, e: 5 }))\n\
             where {\n\
               base: { a: 1, b: 2, c: 3, d: 4 }\n\
             }";
    let n = W23_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local base = {{ a = 1, b = 2, c = 3, d = 4 }}
            local function dictlen(t)
                local c = 0
                for _ in pairs(t) do c = c + 1 end
                return c
            end
            local acc = 0
            for _ = 0, n - 1 do
                local copy = {{ a = base.a, b = base.b, c = base.c, d = base.d, e = 5 }}
                acc = acc + dictlen(copy)
            end
            return acc
        end"#
    );
    let expected = n * 5;
    run_pair("W23", r, &l, n, expected);
}

/// W24 — list comprehension with predicate filter.
#[test]
fn w24_list_comprehension() {
    let r = "#unstrict\n\
             #import list from \"std/list\"\n\
             #main(Int n) -> Int\n\
             list.sum([x * 2 for x in range(n) if x % 3 == 0])";
    let n = W24_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local s = 0
            for x = 0, n - 1 do
                if x % 3 == 0 then
                    s = s + x * 2
                end
            end
            return s
        end"#
    );
    let count = (n + 2) / 3;
    let last_kept = (count - 1) * 3;
    let sum_kept = last_kept * count / 2;
    let expected = 2 * sum_kept;
    run_pair("W24", r, &l, n, expected);
}

/// W25 — pipe chain through list.map / list.filter / list.sum.
#[test]
fn w25_pipe_chain() {
    let r = "#unstrict\n\
             #import list from \"std/list\"\n\
             #main(Int n) -> Int\n\
             range(n) | list.map((x) => x + 1) | list.filter((x) => x % 2 == 0) | list.sum()";
    let n = W25_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local s = 0
            for x = 0, n - 1 do
                local y = x + 1
                if y % 2 == 0 then
                    s = s + y
                end
            end
            return s
        end"#
    );
    let last_kept = if n % 2 == 0 { n } else { n - 1 };
    let count = last_kept / 2;
    let first_kept = 2;
    let expected = (first_kept + last_kept) * count / 2;
    run_pair("W25", r, &l, n, expected);
}

/// W26 — f-string interpolation per-iter concat.
#[test]
fn w26_fstring_interp() {
    fn decimal_len(n: i64) -> i64 {
        if n == 0 {
            1
        } else {
            (n as f64).log10().floor() as i64 + 1
        }
    }
    let r = "#unstrict\n\
             #main(Int n) -> Int\n\
             range(n).reduce(0, (acc, i) => acc + _len(f\"item ${i} of ${n}\"))";
    let n = W26_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local acc = 0
            for i = 0, n - 1 do
                acc = acc + #string.format("item %d of %d", i, n)
            end
            return acc
        end"#
    );
    let n_len = decimal_len(n);
    let prefix_const = 5 + 4;
    let mut expected: i64 = 0;
    for i in 0..n {
        expected += prefix_const + decimal_len(i) + n_len;
    }
    run_pair("W26", r, &l, n, expected);
}

/// W27 — std/dict stdlib module dispatch.
#[test]
fn w27_stdlib_dict() {
    let r = "#unstrict\n\
             #import dict from \"std/dict\"\n\
             #main(Int n) -> Int\n\
             range(n).reduce(0, (acc, _) => acc + _len(dict.keys({ a: 1, b: 2, c: 3 })))";
    let n = W27_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local function dict_keys(d)
                local out = {{}}
                local i = 0
                for k, _ in pairs(d) do
                    i = i + 1
                    out[i] = k
                end
                return out
            end
            local acc = 0
            for _ = 0, n - 1 do
                local d = {{ a = 1, b = 2, c = 3 }}
                acc = acc + #dict_keys(d)
            end
            return acc
        end"#
    );
    let expected = n * 3;
    run_pair("W27", r, &l, n, expected);
}

/// W28 — float div / mod / Int→Float mixed ops; Float return checked
/// within absolute tolerance.
#[test]
fn w28_float_mixed_ops() {
    let r = "#unstrict\n\
             #main(Int n) -> Float\n\
             range(n).reduce(0.0, (acc, i) => acc + i / 3.0 + i % 7)";
    let n = W28_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local acc = 0.0
            for i = 0, n - 1 do
                acc = acc + i / 3.0 + i % 7
            end
            return acc
        end"#
    );
    let mut expected = 0.0_f64;
    let mut i: i64 = 0;
    while i < n {
        expected = expected + (i as f64) / 3.0 + ((i % 7) as f64);
        i += 1;
    }
    run_pair_float("W28", r, &l, n, expected, W28_FLOAT_TOL);
}

/// W30 — strict-mode baseline (typed lambda param). Same algorithm as
/// W6 but the source omits `#unstrict` and the lambda carries a typed
/// param `(Int i)`.
#[test]
fn w30_strict_typed_lambda() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Int\n\
             list.sum(range(n).map((Int i) => i + 1))";
    let n = W30_N;
    let l = format!(
        r#"return function()
            local n = {n}
            local arr = {{}}
            for i = 1, n do arr[i] = i end
            local sum = 0
            for i = 1, n do sum = sum + arr[i] end
            return sum
        end"#
    );
    let expected = n * (n + 1) / 2;
    run_pair("W30", r, &l, n, expected);
}
