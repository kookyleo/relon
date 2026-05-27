//! Phase Z lowering: Relon IR -> WebAssembly MVP 1.0 bytecode.
//!
//! This crate is the *canonical lowered form* layer. The design and ABI
//! contract live in `docs/internal/phase-z-design.md`; this crate only
//! implements the emit side. The host-side runtime that interprets the
//! emitted modules lives in `relon-wasm-evaluator`.
//!
//! ## Z.1 POC scope
//!
//! Per the design doc §10.2 honest deliverable list, Z.1 lowers a closed-
//! form path for **W1, W6, W12** — the int-only fast paths that exercise
//! the host imports ABI's range + sum + arithmetic surface end-to-end.
//! The remaining 9 cmp_lua workloads have lowering sketches in the design
//! doc and stay tracked as Z.3 follow-ups. None of them are silently
//! deferred — every `wN` outside the Z.1 surface returns
//! [`LowerError::ScopeCut`] so callers see the gap immediately.
//!
//! ## Why not lower from `relon_ir::Op` 1:1 in Z.1
//!
//! The full IR Op surface includes raw-memory record ops
//! (`Op::EmitTailRecordFromAbsoluteAddr`, `Op::LoadFieldAtAbsolute`, ...)
//! bound to the cranelift / LLVM `ArenaState` GEP model. WASM linear
//! memory speaks a different addressing convention and the existing
//! ops would force a hand-translation of every record-store sequence
//! into `__relon_arena_alloc` + host-import writes. That's a Z.3+ task.
//!
//! For Z.1 the lowering input is a small high-level [`WasmProgram`]
//! shape — one enum variant per workload — that the host (here
//! `relon-wasm-evaluator`) classifies from the parsed AST. This keeps
//! the POC's emit path short (one fn per variant) and lets us validate
//! the host imports ABI before we sink weeks into a full IR walk.
//!
//! Z.3 will replace the variant-per-workload shape with a real
//! `lower(ir: &IrModule) -> Vec<u8>` walker.

#![deny(unsafe_code)]
#![deny(missing_docs)]

mod host_abi;
mod programs;

use thiserror::Error;

pub use host_abi::{HostImport, HOST_IMPORTS};
pub use programs::{const_segment_end, WasmProgram};

/// Lowering error surface. Each variant carries enough context for the
/// caller (typically `relon-wasm-evaluator::WasmEvaluator::new`) to
/// either route the source to the Z.3 follow-up queue or surface a
/// hard error to the host.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LowerError {
    /// The workload's lowering path is on the Z.3 roadmap (see
    /// `docs/internal/phase-z-design.md` §10.2). The carrier text is a
    /// short workload tag (`"W7-fib"`, etc.) so test logs identify
    /// the scope-cut row without scanning the source.
    #[error("phase Z.1 scope-cut: {0}")]
    ScopeCut(&'static str),

    /// The emit path emitted a malformed module. Wraps the wasm-encoder
    /// or wasmparser error text; reaching this branch is a bug in the
    /// emitter, not a user-facing failure mode.
    #[error("emit produced invalid module: {0}")]
    Invalid(String),
}

/// Emit a complete WASM module for the supplied program.
///
/// Returns the binary representation ready to feed into
/// `wasmtime::Module::new(&engine, bytes)`. The host registers the §4
/// imports via [`HOST_IMPORTS`] before instantiating.
///
/// Module shape (Z.1):
///
/// - 1 memory (initial 16 pages, max unbounded).
/// - Imports per the program's `imports()` slice (a subset of §4).
/// - One exported function: `__main`.
/// - Type signature of `__main` matches the program's
///   `main_signature()`.
///
/// See `docs/internal/phase-z-design.md` §3 for the linear-memory layout
/// and §4 for the host imports ABI.
pub fn lower(program: &WasmProgram) -> Result<Vec<u8>, LowerError> {
    programs::lower_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: emit + parse W1's module round-trip cleanly.
    #[test]
    fn lower_w1_round_trips_wasmparser() {
        let bytes = lower(&WasmProgram::W1IntSumRange).expect("emit W1");
        // wasmparser validates the binary format. A bad emit (wrong
        // section ordering, missing function body, etc.) fails here.
        let mut validator = wasmparser::Validator::new();
        validator
            .validate_all(&bytes)
            .expect("wasmparser validates W1");
    }

    #[test]
    fn lower_w6_round_trips() {
        let bytes = lower(&WasmProgram::W6ListSumPlusOne).expect("emit W6");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W6");
    }

    #[test]
    fn lower_w12_round_trips() {
        let bytes = lower(&WasmProgram::W12IncrementInt).expect("emit W12");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W12");
    }

    #[test]
    fn lower_w2_round_trips() {
        let bytes = lower(&WasmProgram::W2DotProduct).expect("emit W2");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W2");
    }

    #[test]
    fn lower_w9_inline_round_trips() {
        let bytes = lower(&WasmProgram::W9NestedMatrixInline).expect("emit W9 inline");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W9 inline");
    }

    #[test]
    fn lower_w10_inline_round_trips() {
        let bytes = lower(&WasmProgram::W10ConfigEvalInline).expect("emit W10 inline");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W10 inline");
    }

    #[test]
    fn lower_w3_inline_round_trips() {
        let bytes = lower(&WasmProgram::W3StringConcatInline).expect("emit W3 inline");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W3 inline");
    }

    #[test]
    fn lower_w4_round_trips() {
        let bytes = lower(&WasmProgram::W4StringContains { long: false }).expect("emit W4");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W4");
    }

    #[test]
    fn lower_w4_long_round_trips() {
        let bytes = lower(&WasmProgram::W4StringContains { long: true }).expect("emit W4_long");
        wasmparser::Validator::new()
            .validate_all(&bytes)
            .expect("wasmparser validates W4_long");
    }

    #[test]
    fn w4_const_segment_end_covers_both_records() {
        // Short haystack: header(4) + payload(3) = 7. Needle pads
        // to align-4 = offset 16 + 8 = 24, then header(4) + payload(1)
        // = 28, rounded to 32.
        let short_end = const_segment_end(&WasmProgram::W4StringContains { long: false });
        assert!(
            short_end > 16 + 4 + 3 + 4,
            "W4 short const segment must cover both records (got {short_end})"
        );
        // Long haystack: header(4) + payload(256) = 260, needle at
        // offset 16+260 = 276 (already aligned), header(4) + payload(1)
        // = 281, rounded to 288.
        let long_end = const_segment_end(&WasmProgram::W4StringContains { long: true });
        assert!(
            long_end > 16 + 4 + 256 + 4,
            "W4_long const segment must cover both records (got {long_end})"
        );
        // Non-W4 programs report zero (no const segment).
        assert_eq!(const_segment_end(&WasmProgram::W1IntSumRange), 0);
    }

    /// Sanity: every scope-cut workload returns the named cut.
    #[test]
    fn scope_cut_workloads_surface_explicitly() {
        for (prog, tag) in [
            (WasmProgram::W3StringConcat, "W3-string-concat"),
            (WasmProgram::W5DictAccess, "W5-dict-access"),
            (WasmProgram::W7FibRecursion, "W7-fib-recursion"),
            (
                WasmProgram::W8PolymorphicDispatch,
                "W8-polymorphic-dispatch",
            ),
            (WasmProgram::W9NestedMatrix, "W9-nested-matrix"),
            (WasmProgram::W10ConfigEval, "W10-config-eval"),
        ] {
            match lower(&prog) {
                Err(LowerError::ScopeCut(t)) => assert_eq!(t, tag),
                other => panic!("expected ScopeCut({tag}) for {prog:?}, got {other:?}"),
            }
        }
    }
}
