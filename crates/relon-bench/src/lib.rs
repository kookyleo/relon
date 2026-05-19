#![forbid(unsafe_code)]

//! Library surface for the relon-bench crate.
//!
//! Historically `relon-bench` was a pure binary crate (`main.rs` +
//! `bin/profile_alloc.rs` + criterion `benches/*`). v6-λ-0 (bench
//! methodology hardening) introduces a small Rust-side library so the
//! criterion-JSON post-processing (`bench_stats`) is reusable from both
//! integration tests (`tests/methodology_validators.rs`) and a CLI
//! helper (`bin/bench_stats`). The library is `forbid(unsafe_code)` —
//! all unsafe lives in the criterion benches themselves (which need to
//! invoke JIT-compiled trace fns through extern-C entry points).

pub mod bench_stats;
