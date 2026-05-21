//! Short-string optimization (SSO) micro-bench — Phase 1b.
//!
//! Targets the `SmolStr`-backed `Value::String` introduced in
//! `crates/relon-eval-api/src/smol_str.rs`. Two row groups:
//!
//! * **construct_short** — direct `SmolStr` / `String` construction
//!   from a `&str` literal across short / cap-boundary / long
//!   lengths. Measures the cost of the per-string heap-alloc the SSO
//!   path eliminates for payloads <= 22 bytes.
//!
//! * **clone_short** — `clone()` cost on already-constructed `SmolStr`
//!   vs `String` short payloads. SSO inline clone is a 24-byte memcpy
//!   with no allocator hit; `String` clone walks the allocator twice
//!   (alloc + memcpy).
//!
//! * **concat_short** — chained `acc = acc + leaf` concat through the
//!   tree-walker arithmetic path on payloads that stay <=22 bytes the
//!   whole loop. This is the W3-style hot shape we expect the SSO
//!   variant to win on. The pre-SSO baseline is not directly reachable
//!   from this bench (the runtime now uses `SmolStr` unconditionally),
//!   so we compare a "raw `String + &str`" reference implementation in
//!   the same row group as a sanity ceiling — the SSO row should land
//!   inside the same alloc-free envelope.
//!
//! Run: `cargo bench -p relon-bench --bench sso`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use relon_eval_api::SmolStr;

/// Payload sizes that stress the three SSO regimes:
///
/// * 7 bytes — well inside the inline cap; SSO win is pure allocator
///   skip.
/// * 22 bytes — exactly the inline boundary.
/// * 32 bytes — first heap-path payload; SSO falls back to `Arc<str>`
///   but clone is still cheap.
const PAYLOAD_LENGTHS: &[usize] = &[7, 22, 32];

fn payload(len: usize) -> String {
    "abcdefghijklmnopqrstuvwxyz0123456789"
        .chars()
        .cycle()
        .take(len)
        .collect()
}

/// `construct_short` row group — measures per-call construction cost.
fn bench_construct(c: &mut Criterion) {
    let mut group = c.benchmark_group("sso/construct_short");
    for &n in PAYLOAD_LENGTHS {
        let raw = payload(n);
        group.throughput(Throughput::Bytes(n as u64));

        group.bench_with_input(BenchmarkId::new("SmolStr", n), &raw, |b, src| {
            b.iter(|| {
                let s: SmolStr = black_box(src.as_str()).into();
                black_box(s)
            })
        });

        group.bench_with_input(BenchmarkId::new("String", n), &raw, |b, src| {
            b.iter(|| {
                let s: String = black_box(src.as_str()).to_owned();
                black_box(s)
            })
        });
    }
    group.finish();
}

/// `clone_short` row group — measures per-call clone cost on already-
/// constructed payloads. Pre-constructs the source outside the timed
/// loop so the measurement only captures the clone path itself.
fn bench_clone(c: &mut Criterion) {
    let mut group = c.benchmark_group("sso/clone_short");
    for &n in PAYLOAD_LENGTHS {
        let raw = payload(n);
        let smol = SmolStr::from(raw.as_str());
        let owned = raw.clone();
        group.throughput(Throughput::Bytes(n as u64));

        group.bench_with_input(BenchmarkId::new("SmolStr", n), &smol, |b, src| {
            b.iter(|| black_box(src.clone()))
        });

        group.bench_with_input(BenchmarkId::new("String", n), &owned, |b, src| {
            b.iter(|| black_box(src.clone()))
        });
    }
    group.finish();
}

/// `concat_short` row group — chained `acc + leaf` over fixed-length
/// leaves where the running accumulator either stays inside the inline
/// cap (LEAF_SHORT case) or grows past it after a few iterations
/// (LEAF_GROWS case). The SSO win is purely allocation-side, so we
/// measure two reference implementations to bound the expected gain:
///
/// * `smol_concat_via_format` — what the tree-walker arithmetic path
///   actually does (`format!("{}{}", a, b).into()`).
/// * `string_concat_via_push` — plain `String::push_str` baseline,
///   useful as a "no-SSO" ceiling (no enum dispatch, no `format!`
///   intermediary).
const CONCAT_LEAVES_SHORT: &[&str] = &["a", "b", "c", "d"];
const CONCAT_ITERS_SHORT: usize = 5;

fn smol_concat_via_format(leaves: &[&str], iters: usize) -> SmolStr {
    let mut acc = SmolStr::new_empty();
    for _ in 0..iters {
        for leaf in leaves {
            // Pre-`SmolStr::concat` baseline: route through `format!`
            // (`arithmetic::Operator::Add` did this until the typed
            // `String + String` shortcut landed).
            acc = format!("{}{}", acc.as_str(), leaf).into();
        }
    }
    acc
}

fn smol_concat_via_concat(leaves: &[&str], iters: usize) -> SmolStr {
    let mut acc = SmolStr::new_empty();
    for _ in 0..iters {
        for leaf in leaves {
            // Hot path the evaluator now uses for `String + String`:
            // direct two-slice compose into the inline slot, no
            // `format!` / intermediate `String` allocation.
            acc = SmolStr::concat(acc.as_str(), leaf);
        }
    }
    acc
}

fn string_concat_via_push(leaves: &[&str], iters: usize) -> String {
    let mut acc = String::new();
    for _ in 0..iters {
        for leaf in leaves {
            // Reference baseline. No `format!`, no enum wrap.
            let mut next = String::with_capacity(acc.len() + leaf.len());
            next.push_str(&acc);
            next.push_str(leaf);
            acc = next;
        }
    }
    acc
}

fn bench_concat(c: &mut Criterion) {
    let mut group = c.benchmark_group("sso/concat_short");
    let leaves = black_box(CONCAT_LEAVES_SHORT);

    group.bench_function("SmolStr_format", |b| {
        b.iter(|| smol_concat_via_format(black_box(leaves), CONCAT_ITERS_SHORT))
    });

    group.bench_function("SmolStr_concat", |b| {
        b.iter(|| smol_concat_via_concat(black_box(leaves), CONCAT_ITERS_SHORT))
    });

    group.bench_function("String_push_baseline", |b| {
        b.iter(|| string_concat_via_push(black_box(leaves), CONCAT_ITERS_SHORT))
    });

    group.finish();
}

criterion_group!(benches, bench_construct, bench_clone, bench_concat);
criterion_main!(benches);
