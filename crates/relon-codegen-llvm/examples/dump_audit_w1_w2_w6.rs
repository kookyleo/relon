//! Audit helper (2026-05-28, honesty audit): dump post-O3 LLVM IR for the
//! W1 / W2 / W6 LLVM AOT source variants the cmp_lua bench would feed to
//! `LlvmAotEvaluator::from_source`. We need to verify whether LLVM at O3
//! folds the analytic-sum loop body into a closed-form `n*(n+1)/2`-style
//! formula — if so, the relon_llvm_aot* rows for these labels are paper
//! wins (the LuaJIT row pays per-iter add cost while the relon row pays
//! O(1) arithmetic).
//!
//! Usage:
//!   RELON_LLVM_DUMP_DIR=/tmp/audit_w6 \
//!     cargo run -p relon-codegen-llvm --example dump_audit_w1_w2_w6 -- W6
//!
//! Labels: W1 / W2 / W6.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W1_SRC: &str = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";

const W2_SRC: &str = "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

const W6_SRC: &str = "#unstrict\n\
     #import list from \"std/list\"\n\
     #main(Int n) -> Int\n\
     list.sum(range(n).map((i) => i + 1))";

fn main() {
    let dump_dir = std::env::var("RELON_LLVM_DUMP_DIR").unwrap_or_else(|_| {
        eprintln!("set RELON_LLVM_DUMP_DIR=<path> before running");
        std::process::exit(2);
    });
    let label = std::env::args().nth(1).unwrap_or_else(|| "W6".to_string());
    let (src, n_default): (&str, i64) = match label.as_str() {
        "W1" => (W1_SRC, 10_000),
        "W2" => (W2_SRC, 1_000),
        "W6" => (W6_SRC, 10_000),
        other => {
            eprintln!("unknown label `{other}` — expected W1 / W2 / W6");
            std::process::exit(2);
        }
    };
    eprintln!("Compiling {label} source via LlvmAotEvaluator::from_source ...");
    let ev = LlvmAotEvaluator::from_source(src).expect("from_source");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n_default));
    let got = ev.run_main(args).expect("run_main");
    eprintln!("{label} result: {got:?}");
    eprintln!("Artifacts written to: {dump_dir}/module.post_o3.ll");
}
