//! `#161` write-to-buffer micro-bench.
//!
//! Targets the `SmolStr::try_build_inline` fast path the stdlib
//! `to_lower` / `to_upper` / `title` / `concat` helpers now route
//! through for short ASCII payloads. Each row pits the new inline-write
//! shape (`fold_baseline_string_wrap`) against the simulated pre-#161
//! shape (`fold_via_string_with_capacity`) so the delta isolates the
//! `String::with_capacity` + `Arc::from(String)` allocator round-trip
//! the historical path paid even when the output fit inside the 22-byte
//! inline slot.
//!
//! Two row groups:
//!
//! * **stdlib/to_lower_inline** — ASCII payloads at 5 / 12 / 22 byte
//!   lengths. All three land inside the `SmolStr` inline cap, so the
//!   new path stays alloc-free. The 32-byte row is included as a
//!   ceiling check: both paths go to heap there, the delta should
//!   collapse to noise.
//!
//! * **stdlib/concat_inline** — `SmolStr::concat(a, b)` vs the
//!   pre-#161 `String::with_capacity` shape `StringConcat` used. Same
//!   payload axis — short rows benefit from the inline slot, the
//!   32-byte row is the heap-path ceiling.
//!
//! Run: `cargo bench -p relon-bench --bench string_stdlib`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use relon_eval_api::{SmolStr, SMOL_STR_INLINE_CAP};

/// Payload sizes:
///
/// * `5`  — typical short identifier (`"hello"`, `"world"`, dict keys).
/// * `12` — mid-inline range, still well under the 22-byte cap.
/// * `22` — exact inline-cap boundary; new path still alloc-free.
/// * `32` — past the cap; both paths go to heap (ceiling check).
const PAYLOAD_LENGTHS: &[usize] = &[5, 12, 22, 32];

/// Build an ASCII payload of `len` mixed upper/lower letters so the
/// `to_lower` mask + xor body has real work to do.
fn ascii_mixed(len: usize) -> String {
    "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"
        .chars()
        .cycle()
        .take(len)
        .collect()
}

/// Pre-#161 shape: build a `String::with_capacity(s.len())`, write the
/// folded bytes, then hand the buffer to `SmolStr::from(String)`.
/// Matches the historical `fold_string(...).into()` cost on short ASCII
/// inputs.
#[inline(never)]
fn to_lower_via_string_with_capacity(s: &str) -> SmolStr {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let in_range = b.wrapping_sub(b'A') < 26;
        out.push((b ^ if in_range { 0x20 } else { 0x00 }) as char);
    }
    SmolStr::from(out)
}

/// `#161` shape: write directly into the inline slot when the payload
/// fits, fall back to the heap path otherwise. Matches the new
/// `fold_string_to_smol` body for the `Lower` ASCII fast path.
#[inline(never)]
fn to_lower_via_inline_write(s: &str) -> SmolStr {
    let bytes = s.as_bytes();
    if bytes.len() <= SMOL_STR_INLINE_CAP && s.is_ascii() {
        if let Some(smol) = SmolStr::try_build_inline(bytes.len(), |out| {
            for (i, &b) in bytes.iter().enumerate() {
                let in_range = b.wrapping_sub(b'A') < 26;
                out[i] = b ^ if in_range { 0x20 } else { 0x00 };
            }
        }) {
            return smol;
        }
    }
    // Heap fallback — same shape as the with-capacity path.
    let mut out = String::with_capacity(s.len());
    for &b in bytes {
        let in_range = b.wrapping_sub(b'A') < 26;
        out.push((b ^ if in_range { 0x20 } else { 0x00 }) as char);
    }
    SmolStr::from(out)
}

fn bench_to_lower(c: &mut Criterion) {
    let mut group = c.benchmark_group("stdlib/to_lower_inline");
    for &n in PAYLOAD_LENGTHS {
        let payload = ascii_mixed(n);
        group.throughput(Throughput::Bytes(n as u64));

        group.bench_with_input(BenchmarkId::new("inline_write", n), &payload, |b, src| {
            b.iter(|| {
                let s = to_lower_via_inline_write(black_box(src.as_str()));
                black_box(s)
            })
        });

        group.bench_with_input(
            BenchmarkId::new("string_with_capacity", n),
            &payload,
            |b, src| {
                b.iter(|| {
                    let s = to_lower_via_string_with_capacity(black_box(src.as_str()));
                    black_box(s)
                })
            },
        );
    }
    group.finish();
}

/// Pre-#161 `StringConcat` body: `String::with_capacity` + two
/// `push_str` + `SmolStr::from(String)`.
#[inline(never)]
fn concat_via_string_with_capacity(a: &str, b: &str) -> SmolStr {
    let mut out = String::with_capacity(a.len() + b.len());
    out.push_str(a);
    out.push_str(b);
    SmolStr::from(out)
}

fn bench_concat(c: &mut Criterion) {
    let mut group = c.benchmark_group("stdlib/concat_inline");
    for &n in PAYLOAD_LENGTHS {
        // Two halves of `n / 2` each (rounded down). Keeps the total at
        // the bench's nominal length without splitting the payload axis.
        let half = n / 2;
        let lhs = ascii_mixed(half);
        let rhs = ascii_mixed(n - half);
        group.throughput(Throughput::Bytes(n as u64));

        let pair = (lhs.clone(), rhs.clone());
        group.bench_with_input(BenchmarkId::new("smol_concat", n), &pair, |b, (a, c)| {
            b.iter(|| {
                let s = SmolStr::concat(black_box(a.as_str()), black_box(c.as_str()));
                black_box(s)
            })
        });

        group.bench_with_input(
            BenchmarkId::new("string_with_capacity", n),
            &pair,
            |b, (a, c)| {
                b.iter(|| {
                    let s = concat_via_string_with_capacity(
                        black_box(a.as_str()),
                        black_box(c.as_str()),
                    );
                    black_box(s)
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_to_lower, bench_concat);
criterion_main!(benches);
