//! Capability + correctness proof for the runtime-host MCJIT target.
//!
//! The MCJIT execution engine takes no MCPU, so before this fix the
//! X86 backend lowered every JIT'd function for **generic x86-64** and
//! dropped the host `SlowDivide64` tuning — an i64 `%` / `/` came out
//! as a bare microcoded `idivq` instead of the host
//! `shr $32; je; divl` 32-bit-narrowing fast path. The fix stamps the
//! host `"target-cpu"` / `"target-features"` (queried at runtime, never
//! hard-coded) onto every function so MCJIT lowers for the CPU it runs
//! on.
//!
//! These tests prove three things, on whatever host runs them:
//!  1. the stamped `target-cpu` equals the runtime host CPU (not a
//!     literal);
//!  2. the in-memory MCJIT machine code for an i64 `%` now contains the
//!     host divide-narrowing (a 32-bit `div`/`idiv`), which generic
//!     x86-64 codegen never emits for a 64-bit remainder; and
//!  3. results stay bit-identical (oracle-verified) — host codegen is a
//!     correctness-preserving instruction-selection change.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Minimal i64 remainder kernel: `#main(Int a, Int b) -> Int : a % b`.
/// The `%` lowers to a single `srem i64` whose backend lowering is the
/// thing the host-CPU stamp changes.
const REM_SRC: &str = "#main(Int a, Int b) -> Int\n a % b";

fn extract_int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        other => panic!("expected Int, got {other:?}"),
    }
}

/// (1) The stamped `target-cpu` is the runtime host, never a hard-coded
/// microarchitecture. The post-O3 IR dump carries the attribute on the
/// body functions; assert it matches `host_target_cpu()` exactly.
#[test]
fn stamped_target_cpu_equals_runtime_host() {
    let host = LlvmAotEvaluator::host_target_cpu();
    assert!(
        !host.is_empty(),
        "host CPU introspection returned empty; cannot verify the stamp"
    );
    let ev = LlvmAotEvaluator::from_source(REM_SRC).expect("rem kernel compiles via LLVM AOT");
    let dump = ev.emit_ir_dump();
    let needle = format!("\"target-cpu\"=\"{host}\"");
    assert!(
        dump.contains(&needle),
        "post-O3 IR does not stamp the runtime host `target-cpu` ({needle}); \
         dump attributes:\n{}",
        dump.lines()
            .filter(|l| l.contains("attributes #"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // Guard against a regression that hard-codes a CPU literal: the
    // value the source pins MUST be the runtime query, so anything that
    // is not `host` and looks like a CPU literal would be a red flag.
    // (We can only positively assert the host value is present, which
    // the check above does.)
    assert!(
        !dump.contains("\"target-cpu\"=\"\""),
        "a function was stamped with an empty target-cpu:\n{dump}"
    );
}

/// Scan a tiny window of x86-64 machine code for a 32-bit `div`/`idiv`
/// (`F7 /6` or `F7 /7`) whose `ModR/M` is NOT preceded by a `REX.W`
/// (`0x48`) prefix byte. Generic x86-64 codegen lowers an i64 `%` to a
/// single REX.W `idivq` (`48 F7 /7`); the host `SlowDivide64` narrowing
/// instead branches to a 32-bit divide in the fast arm. So the presence
/// of a non-REX-W `F7 /6|/7` in the body is the fingerprint of the host
/// narrowing.
fn has_32bit_divide(code: &[u8]) -> bool {
    for i in 0..code.len() {
        if code[i] != 0xF7 {
            continue;
        }
        let Some(&modrm) = code.get(i + 1) else {
            continue;
        };
        // ModR/M reg field (bits 5:3): 6 = DIV, 7 = IDIV.
        let reg = (modrm >> 3) & 0x7;
        if reg != 6 && reg != 7 {
            continue;
        }
        // A REX.W prefix would be the byte immediately before the F7
        // opcode (no other prefix sits between REX and the opcode here).
        let rex_w = i > 0 && (code[i - 1] & 0xF8) == 0x48 && (code[i - 1] & 0x08) != 0;
        if !rex_w {
            return true;
        }
    }
    false
}

/// True if the window contains the high-32-bits narrowing test
/// `shr <reg>, 0x20` (`48 C1 /5 20`) that gates the host fast path.
fn has_shr_32_test(code: &[u8]) -> bool {
    for w in code.windows(4) {
        // 48 C1 E? 20  ==  shr r64, 0x20  (ModR/M reg field = 5)
        if w[0] == 0x48 && w[1] == 0xC1 && (w[2] >> 3) & 0x7 == 5 && w[3] == 0x20 {
            return true;
        }
    }
    false
}

/// (2) Mechanism: the RUNTIME MCJIT machine code for `a % b` contains
/// the host divide-narrowing fast path (a 32-bit divide + the
/// `shr $32` high-bits test), which generic-x86-64 lowering of an i64
/// `srem` never emits. This reads the bytes the engine actually placed
/// in executable memory at the resolved entry address.
#[test]
fn mcjit_rem_uses_host_divide_narrowing() {
    let ev = LlvmAotEvaluator::from_source(REM_SRC).expect("rem kernel compiles via LLVM AOT");
    // Prefer the typed fast entry (a tight `(i64,i64)->i64` body that is
    // just the remainder); fall back to the buffer entry otherwise.
    let addr = ev
        .fast_entry_runtime_addr()
        .unwrap_or_else(|| ev.entry_runtime_addr());
    assert!(addr != 0, "JIT did not resolve an entry address");

    // The body is tiny (< 64 bytes); 256 bytes is a generous, safe
    // window inside the function's own code page.
    // SAFETY: `addr` is a live, executable JIT function pointer owned by
    // `ev` for its lifetime; reading its own code bytes is in-bounds.
    let code: &[u8] = unsafe { std::slice::from_raw_parts(addr as *const u8, 256) };

    assert!(
        has_32bit_divide(code),
        "MCJIT code for `a % b` contains no 32-bit div/idiv — the host \
         idiv-narrowing fast path is absent (generic bare idivq). \
         host_cpu={}",
        LlvmAotEvaluator::host_target_cpu()
    );
    assert!(
        has_shr_32_test(code),
        "MCJIT code for `a % b` lacks the `shr <reg>,0x20` high-bits test \
         that gates the host narrowing. host_cpu={}",
        LlvmAotEvaluator::host_target_cpu()
    );
}

/// (3) Correctness: host codegen must produce bit-identical results.
/// Oracle = the same `%` in plain Rust, across positive / negative /
/// boundary divisors (the 32-bit fast arm vs the 64-bit slow arm both
/// get exercised).
#[test]
fn mcjit_rem_matches_rust_oracle() {
    let ev = LlvmAotEvaluator::from_source(REM_SRC).expect("rem kernel compiles via LLVM AOT");
    let cases: &[(i64, i64)] = &[
        (10, 3),
        (10, 5),
        (0, 7),
        (7, 7),
        (1_000_003, 101),
        // Operands that do NOT fit in 32 bits -> exercises the 64-bit
        // slow arm of the narrowing branch.
        (9_000_000_000, 7),
        (-17, 5),
        (17, -5),
        (-17, -5),
        (i64::MAX, 3),
        (i64::MIN + 1, 7),
    ];
    for &(a, b) in cases {
        let mut args = HashMap::new();
        args.insert("a".to_string(), Value::Int(a));
        args.insert("b".to_string(), Value::Int(b));
        let got = extract_int(ev.run_main(args).expect("run_main"));
        assert_eq!(
            got,
            a % b,
            "MCJIT host-codegen `a % b` diverged from Rust oracle for a={a}, b={b}"
        );
    }
}
