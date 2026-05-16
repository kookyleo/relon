#![forbid(unsafe_code)]

use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    println!("--- Relon Performance Benchmark ---");

    // 1. Boot & Simple Eval (Cold/Warm Start)
    let source = "{ val: 1 + 2 * 3 / 4.0 }";
    let (parse_time, eval_time) = bench_once(source);
    println!("Simple Arithmetic ('{}'):", source);
    println!("  Parse: {:?}", parse_time);
    println!("  Eval : {:?}", eval_time);
    println!("  Total: {:?}", parse_time + eval_time);

    // 2. Complex Logic (Loops/Comprehension)
    let source_complex = r#"{
        "list": [x * 2 for x in range(1000) if x % 2 == 0],
        "check": &sibling.list
    }"#;
    let (parse_time, eval_time) = bench_once(source_complex);
    println!("\nComplex Logic (Range 1000 + Sum):");
    println!("  Parse: {:?}", parse_time);
    println!("  Eval : {:?}", eval_time);

    // 3. Iterative Average (Steady State)
    let iterations = 1000;
    let mut total_parse = Duration::ZERO;
    let mut total_eval = Duration::ZERO;

    for _ in 0..iterations {
        let (p, e) = bench_once(source);
        total_parse += p;
        total_eval += e;
    }

    println!("\nAverage over {} iterations (Simple):", iterations);
    println!("  Mean Parse: {:?}", total_parse / iterations);
    println!("  Mean Eval : {:?}", total_eval / iterations);
}

fn bench_once(source: &str) -> (Duration, Duration) {
    let start_parse = Instant::now();
    let ast = parse_document(source).expect("Parse failed");
    let parse_time = start_parse.elapsed();

    let start_eval = Instant::now();
    let ctx = {
        let mut ctx = Context::new().with_root(ast.clone());
        ctx.prepend_module_resolver(Arc::new(relon_evaluator::StdModuleResolver));
        Arc::new({
            let mut ctx = ctx;
            relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
            ctx
        })
    };
    let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
    let _result = eval
        .eval(&ast, &Arc::new(Scope::default()))
        .expect("Eval failed");
    let eval_time = start_eval.elapsed();

    (parse_time, eval_time)
}
