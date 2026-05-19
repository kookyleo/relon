//! v3++ item 4: SIMD ASCII fast-path throughput bench.
//!
//! Compares two implementations of "fold ASCII bytes" across three
//! corpus sizes (100 B / 1 KB / 10 KB) and three modes (Upper / Lower
//! / Title):
//!
//! * **baseline_scalar_fold_string** — re-implements the pre-v3++-item-4
//!   fold path on ASCII bytes. Walks the input codepoint-by-codepoint
//!   (each ASCII byte is one codepoint), per-byte routes through
//!   `char::to_uppercase` / `char::to_lowercase` for cased letters
//!   and the identity path for everything else. This is the exact
//!   shape `relon-evaluator::stdlib::fold_string` followed before the
//!   fast path was wired in (sans the table lookups, which would all
//!   miss on ASCII anyway). The intent is to capture the per-byte
//!   decode + branch + `String::push` overhead that the fast path
//!   eliminates.
//!
//! * **simd_fold_string** — calls
//!   `relon_ir::ascii_fold_simd::fold_ascii_prefix_into_string`, the
//!   helper now wired into `fold_string`. On wasm32 + simd128 it
//!   emits v128 mask + xor opcodes; on x86_64 / aarch64 it falls back
//!   to a scalar loop that LLVM autovectorises. This bench runs on
//!   the native host (x86_64) so the autovec path is what's exercised
//!   here; the wasm32 measurements live in
//!   `docs/internal/wasm-bench-report-2026-05-16.md` appendix A.24.
//!
//! Both rows produce byte-identical output on the inputs used here
//! (the corpus generator emits printable ASCII only). The wins come
//! from:
//!
//! 1. branch-free mask-and-xor vs per-byte `match` cascade
//! 2. bulk `Vec::resize` + `dst[i] =` write vs `String::push(char)`
//!    (which calls `char::encode_utf8` even for ASCII)
//! 3. no per-byte `chars().next()` iterator state
//!
//! Run: `cargo bench -p relon-bench --bench ascii_case_fold`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use relon_ir::ascii_fold_simd::{fold_ascii_prefix_into_string, AsciiFoldMode};

/// Build an ASCII corpus of `n` bytes that exercises both the in-
/// range (cased letters) and out-of-range (digits / punctuation /
/// whitespace) halves of the SIMD mask. Deterministic for
/// reproducibility across runs.
fn build_ascii_corpus(n: usize) -> String {
    // 32-char palette: A-Z (26) + 4 digits + 2 punct. Cycle through
    // it; the period is coprime with the v128 chunk size (16) so the
    // mask hot path sees varied bytes across chunks.
    const PALETTE: &[u8] = b"AbCdEfGhIjKlMnOpQrStUvWxYz01 ., 9";
    debug_assert_eq!(PALETTE.len(), 33);
    let mut s = String::with_capacity(n);
    for i in 0..n {
        s.push(PALETTE[i % PALETTE.len()] as char);
    }
    s
}

/// Baseline scalar fold: mirrors the per-byte-codepoint code shape
/// the pre-fast-path `fold_string` used on ASCII input. Crucially we
/// route every cased byte through `char::to_uppercase`/`to_lowercase`
/// so the `char::encode_utf8` call inside `String::push` matches the
/// old impl's overhead.
fn baseline_scalar_fold_string(s: &str, mode: AsciiFoldMode) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        match mode {
            AsciiFoldMode::Upper => {
                for u in c.to_uppercase() {
                    out.push(u);
                }
            }
            AsciiFoldMode::Lower => {
                for u in c.to_lowercase() {
                    out.push(u);
                }
            }
            AsciiFoldMode::Title => {
                if c.is_whitespace() {
                    out.push(c);
                    at_word_start = true;
                    continue;
                }
                if at_word_start {
                    for u in c.to_uppercase() {
                        out.push(u);
                    }
                } else {
                    for u in c.to_lowercase() {
                        out.push(u);
                    }
                }
                at_word_start = false;
            }
        }
    }
    out
}

fn simd_fold_string(s: &str, mode: AsciiFoldMode) -> String {
    let mut out = String::with_capacity(s.len());
    let r = fold_ascii_prefix_into_string(s.as_bytes(), mode, true, &mut out);
    // The corpus is all-ASCII so the fast-path consumes everything.
    debug_assert_eq!(r.consumed, s.len());
    out
}

fn bench_ascii_case_fold(c: &mut Criterion) {
    let mut group = c.benchmark_group("ascii_case_fold");

    // Three sizes — small (100 B) captures per-call overhead, 1 KB is
    // a typical config-file string field, 10 KB stresses raw
    // throughput where the SIMD loop's per-chunk amortisation
    // dominates.
    for &n in &[100usize, 1024, 10_240] {
        let corpus = build_ascii_corpus(n);
        group.throughput(Throughput::Bytes(n as u64));

        for &(mode, mode_str) in &[
            (AsciiFoldMode::Upper, "upper"),
            (AsciiFoldMode::Lower, "lower"),
            (AsciiFoldMode::Title, "title"),
        ] {
            // Baseline: per-cp branch + push.
            group.bench_function(BenchmarkId::new(format!("baseline_{mode_str}"), n), |b| {
                b.iter(|| {
                    let out = baseline_scalar_fold_string(black_box(&corpus), mode);
                    black_box(out)
                });
            });

            // SIMD fast-path: mask + xor, autovec on native.
            group.bench_function(BenchmarkId::new(format!("simd_{mode_str}"), n), |b| {
                b.iter(|| {
                    let out = simd_fold_string(black_box(&corpus), mode);
                    black_box(out)
                });
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_ascii_case_fold);
criterion_main!(benches);
