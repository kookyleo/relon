use relon_codegen_cranelift::AotEvaluator;

/// Regression: the range pipeline `range(n).map((i) => "axb").filter((s) => s.contains("x")).len()`
/// (the cmp_lua W4 workload's Relon source) drove `LetSet { idx, ty: String }` through the AOT
/// codegen for the first time. `set_let` previously routed `String` through a `_ => I64` fallback
/// when allocating the Cranelift variable, but the operand stack carries a `String` value as an
/// `i32` arena offset — so the first `def_var` for that slot panicked with
/// `declared type of variable var{N} doesn't match type of value v{M}`. The fix uses
/// `ir_ty_to_cl` for the slot-type lookup so `set_let` agrees with the rest of codegen on
/// `String/List*/Closure → i32`.
#[test]
fn w4_pipeline_aot_compiles() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n)\n\
                 .map((i) => \"axb\")\n\
                 .filter((s) => s.contains(\"x\"))\n\
                 .len()";
    AotEvaluator::from_source(src).expect("W4 range.map.filter.len AOT must compile");
}
