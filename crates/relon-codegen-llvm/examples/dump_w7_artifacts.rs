//! Dump W7 (cmp_lua fib recursion) post-O3 LLVM IR + host-targeted
//! assembly for the LLVM AOT lambda body. Drives
//! `LlvmAotEvaluator::from_source` against the same W7 source the
//! cmp_lua bench uses, then relies on the `RELON_LLVM_DUMP_DIR` env
//! var to write `module.post_o3.ll`, `module.s`, and `module.o`.
//!
//! Usage:
//!   RELON_LLVM_DUMP_DIR=/tmp/relon_w7_dump \
//!     cargo run -p relon-codegen-llvm --example dump_w7_artifacts
//!
//! Inspect:
//!   grep -A 200 'relon_lambda_0' /tmp/relon_w7_dump/module.post_o3.ll
//!   objdump -d --disassemble=relon_lambda_0_closure_0 /tmp/relon_w7_dump/module.o

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W7_SRC: &str = "#main(Int n) -> Dict\n\
                      {\n\
                        #internal\n\
                        fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                        result: fib(n)\n\
                      }";

fn main() {
    let dump_dir = std::env::var("RELON_LLVM_DUMP_DIR").unwrap_or_else(|_| {
        eprintln!("set RELON_LLVM_DUMP_DIR=<path> before running");
        std::process::exit(2);
    });
    eprintln!("Compiling W7 source via LlvmAotEvaluator::from_source ...");
    let ev = LlvmAotEvaluator::from_source(W7_SRC).expect("from_source");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(22));
    let got = ev.run_main(args).expect("run_main");
    eprintln!("fib(22) = {got:?}");
    eprintln!("Artifacts written to: {dump_dir}");
    eprintln!("  - module.post_o3.ll");
    eprintln!("  - module.s");
    eprintln!("  - module.o");

    // Dump first 256 bytes of the JIT-resolved fast-entry symbol so
    // we can disassemble exactly what MCJIT produced at runtime
    // (separate from the static `.s` dump above, which used a fresh
    // TargetMachine and may not match the engine's pipeline byte-for-
    // byte).
    if let Some(fast_addr) = ev.fast_entry_runtime_addr() {
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(fast_addr as *const u8, 1024) };
        let path = std::path::PathBuf::from(&dump_dir).join("runtime.fast_entry.bin");
        std::fs::write(&path, bytes).expect("write fast entry binary");
        eprintln!("  - runtime.fast_entry.bin  (1024 bytes at {fast_addr:#x})");
    }
    // Also the buffer entry (production path).
    let entry_addr = ev.entry_runtime_addr();
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(entry_addr as *const u8, 1024) };
    let path = std::path::PathBuf::from(&dump_dir).join("runtime.entry.bin");
    std::fs::write(&path, bytes).expect("write entry binary");
    eprintln!("  - runtime.entry.bin       (1024 bytes at {entry_addr:#x})");

    // Lambda lives *before* both entries in the JIT code arena (LLVM
    // emits in declaration order: helpers + lambdas first, then the
    // entries). The code arena is a single anonymous mmap; we know
    // the page boundary because we asked for `4096`-aligned regions
    // in `mcjit_mm.rs`. Dump from the page-aligned start of the entry's
    // containing page so we capture the lambda body without reading
    // unmapped memory.
    let page_start = entry_addr & !0xfff;
    let dump_len = (entry_addr - page_start) + 1024;
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(page_start as *const u8, dump_len) };
    let path = std::path::PathBuf::from(&dump_dir).join("runtime.module_block.bin");
    std::fs::write(&path, bytes).expect("write module block binary");
    eprintln!(
        "  - runtime.module_block.bin  ({} bytes from {:#x}, entry at +{:#x}, fast at +{:#x})",
        dump_len,
        page_start,
        entry_addr - page_start,
        ev.fast_entry_runtime_addr()
            .unwrap_or(0)
            .saturating_sub(page_start),
    );
}
