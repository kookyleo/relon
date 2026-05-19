//! v6-λ-machine (2026-05-19): #[ignore]-gated integration test that runs
//! [`relon_bench::quiescence::verify_quiescence`] and reports the result.
//!
//! Run with:
//!     cargo test -p relon-bench --test quiescence_check -- --ignored
//!
//! Why `#[ignore]`: a vanilla `cargo test` round must NOT depend on the
//! host being benched. CI boxes / docker containers / un-tuned dev laptops
//! will fail the gate; we don't want that to gate landing unrelated code.
//! The bench harness itself (`benches/trace_jit_hot_loop.rs`) calls
//! `verify_quiescence` at startup; that's the production enforcement
//! point. This test is a manual debug aid.

use relon_bench::quiescence::{verify_quiescence, FORCE_RUN_ENV};

#[test]
#[ignore = "run with --ignored after `scripts/bench_quiescence.sh`"]
fn verify_machine_quiescence() {
    match verify_quiescence() {
        Ok(report) => {
            eprintln!("\nMachine quiescent. Report:\n{}", report.summary());
            if report.force_run {
                eprintln!(
                    "(NOTE: {FORCE_RUN_ENV} is set; the bench harness will RUN even if a future re-check fails.)"
                );
            }
        }
        Err(err) => {
            // Print every individual failure so the user knows exactly
            // what to fix.
            eprintln!("\n{err}");
            eprintln!("Summary:\n{}", err.report.summary());
            panic!(
                "machine not quiescent — see stderr above. Fix the listed gates or set {FORCE_RUN_ENV}=1 to bypass."
            );
        }
    }
}
