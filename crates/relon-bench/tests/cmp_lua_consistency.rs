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
               #private\n\
               d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
               #private\n\
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
                       #private\n\
                       fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
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
               #private\n\
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
               #private\n\
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

#[test]
fn w10_config_eval() {
    let r = "#import list from \"std/list\"\n\
             #main(Int n) -> Dict\n\
             {\n\
               #private\n\
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
