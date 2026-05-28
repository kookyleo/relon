//! Dump W3 (cmp_lua string-concat reduce) post-O3 LLVM IR + host-targeted
//! assembly for the LLVM AOT lambda body. Drives
//! `LlvmAotEvaluator::from_source` against the same W3 source the
//! cmp_lua bench uses, then relies on the `RELON_LLVM_DUMP_DIR` env
//! var to write `module.post_o3.ll`, `module.s`, and `module.o`.
//!
//! Usage:
//!   RELON_LLVM_DUMP_DIR=/tmp/relon_w3_dump \
//!     cargo run -p relon-codegen-llvm --example dump_w3_artifacts
//!
//! Inspect:
//!   grep -A 200 '@main' /tmp/relon_w3_dump/module.post_o3.ll
//!   less /tmp/relon_w3_dump/module.s
//!
//! Note: the bench label for W3 is `W3_string_concat` and the source
//! mirrors what `llvm_aot_source_for("W3_string_concat")` returns in
//! `crates/relon-bench/benches/cmp_lua.rs` — `#unstrict` prefix +
//! `range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

const W3_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> String\n\
                      range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";

fn main() {
    let dump_dir = std::env::var("RELON_LLVM_DUMP_DIR").unwrap_or_else(|_| {
        eprintln!("set RELON_LLVM_DUMP_DIR=<path> before running");
        std::process::exit(2);
    });
    eprintln!("Compiling W3 source via LlvmAotEvaluator::from_source ...");
    let ev = LlvmAotEvaluator::from_source(W3_SRC).expect("from_source");
    let mut args = HashMap::new();
    let n: i64 = std::env::var("RELON_W3_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);
    args.insert("n".to_string(), Value::Int(n));
    let got = ev.run_main(args).expect("run_main");
    match &got {
        Value::String(s) => eprintln!("W3 result: String(len={})", s.len()),
        other => eprintln!("W3 result: {other:?}"),
    }
    eprintln!("Artifacts written to: {dump_dir}");
    eprintln!("  - module.post_o3.ll");
    eprintln!("  - module.s");
    eprintln!("  - module.o");

    // Capture both fast and entry runtime addresses so we can disassemble
    // exactly what MCJIT produced for the W3 hot loop.
    if let Some(fast_addr) = ev.fast_entry_runtime_addr() {
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(fast_addr as *const u8, 2048) };
        let path = std::path::PathBuf::from(&dump_dir).join("runtime.fast_entry.bin");
        std::fs::write(&path, bytes).expect("write fast entry binary");
        eprintln!("  - runtime.fast_entry.bin  (2048 bytes at {fast_addr:#x})");
    }
    let entry_addr = ev.entry_runtime_addr();
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(entry_addr as *const u8, 2048) };
    let path = std::path::PathBuf::from(&dump_dir).join("runtime.entry.bin");
    std::fs::write(&path, bytes).expect("write entry binary");
    eprintln!("  - runtime.entry.bin       (2048 bytes at {entry_addr:#x})");

    // Module block: capture from page start up to entry+2KB so we see
    // the entire generated lambda body region.
    let page_start = entry_addr & !0xfff;
    let dump_len = (entry_addr - page_start) + 2048;
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
