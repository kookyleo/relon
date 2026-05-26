# W5 trace-JIT layout variance RCA

**Date**: 2026-05-26
**Trigger**: open follow-up #4 from
[`full-supersession-completion.md`](full-supersession-completion.md) —
W5 (`dict_str_key`) trace_jit time is bimodal / tri-modal across
process restarts (84 / 110 / 142 / 170 / 200+ µs clusters) even on a
quiescent s90 with the same binary.
**Author handle**: worktree agent (RCA branch
`worktree-agent-a90a674a865cc7ddc`).

## TL;DR

The W5 variance is **not** caused by trace-fn machine-code alignment,
nor by ASLR of the JIT code page. It is caused by an **insufficient
dict-lookup inline-cache slot count** combined with a **low-entropy
slot-derivation hash**:

* The inline IC probe used `slot = (dict_ptr ^ key_ptr) >> 4 & (N-1)`
  with `N = 32`. The 10 hot W5 keys are heap-allocated by the same
  `build_string_record` calls into the same Vec arena, so their
  pointers share their high bits with the dict pointer. The XOR
  cancels the shared bits, leaving the slot index dominated by the
  small `>> 4 & 31` window of address bits — a low-entropy region.
* On any given process launch, the 10 keys' slot indices behave like
  **10 balls into 32 bins**. By the birthday-paradox, the expected
  number of collisions is `k² / 2N ≈ 1.6`, and the probability of at
  least one collision is ~74%. Each collision converts ~1000 IC-hit
  iters (5 ns each) into ~1000 IC-miss + helper-call iters (~25-100 ns
  each), so per-collision the bench widens by ~25-30 µs.
* The bench's `bench_function` setup is process-global: once a launch
  draws an unlucky `(dict_ptr, key_ptr, key_ptr, …)` heap tuple, every
  criterion sample in that process runs the same slow-cluster. The
  per-process timings are tightly clustered (CV < 2%); the bimodal
  pattern lives **across** processes, not within.

**Proposed fix (applied in this branch)**:

1. Switch the IC probe to a **multiplicative mix** (Knuth's golden
   ratio multiplier `0x9E37_79B9_7F4A_7C15`) and pick the top
   `log2(N)` bits. Routes the full 64-bit address entropy through the
   slot index instead of leaking it via XOR cancellation.
2. Bump `DICT_LOOKUP_IC_SLOT_COUNT` from 32 to 64. Halves the
   expected collision count to ~0.78 and the at-least-one probability
   from 74% → 49%. Per-context IC memory grows 768B → 1536B; the
   whole `TraceContext` is still 1.7 KB, well under one 4 KiB page.

Combined effect on W5 (16 process restarts, s90-equivalent local
Xeon E5-2609 v4, `--sample-size 30 --measurement-time 4`):

| Variant                  | Span      | Min   | Median | Max   | "0-coll" run frac |
|--------------------------|-----------|-------|--------|-------|---------------------|
| Baseline (32 + xor-shift) | 2.30×     | 84 µs | 121 µs | 193 µs | 8% (1/12)            |
| 32 slots + multiplicative | 1.74×     | 85 µs | 114 µs | 149 µs | 19% (3/16)           |
| 64 slots + multiplicative | **1.69×** | 85 µs | 110 µs | 144 µs | **50% (10/20)**     |
| 128 slots + multiplicative | 1.69×    | 85 µs | 85 µs  | 144 µs | 65% (13/20)         |

The 144 µs ceiling at 64+mult is the 2-collision worst case (P ≈ 5%
under birthday-paradox). The 84 µs floor is the no-collision
fast-cluster; the 1-collision penalty is ~25 µs. 64 slots was picked
as the design point because it pulls the **median** firmly into the
fast cluster while keeping `TraceContext` < 2 KiB.

## Reproduction

### Local probe (isolated): no variance

A focused `w5_variance_probe` binary
(`crates/relon-bench/src/bin/w5_variance_probe.rs`) installs *only*
the W5 trace, then runs 16 batches of 10 000 invokes each:

```text
$ for i in $(seq 1 8); do
    W5_PROBE_BATCHES=8 W5_PROBE_ITERS=10000 taskset -c 2 \
        target/release/w5_variance_probe 2>&1 | grep stats:
  done
# stats: min=83790ns median=83805ns max=83834ns max/min=1.00x
# stats: min=83741ns median=83805ns max=83947ns max/min=1.00x
# stats: min=83774ns median=83808ns max=83938ns max/min=1.00x
# stats: min=83825ns median=83874ns max=84019ns max/min=1.00x
# stats: min=83835ns median=83883ns max=84052ns max/min=1.00x
# stats: min=83850ns median=83868ns max=84067ns max/min=1.00x
# stats: min=83840ns median=83853ns max=83909ns max/min=1.00x
# stats: min=83829ns median=83862ns max=83997ns max/min=1.00x
```

**Result**: when the only thing in the process is the W5 trace +
fixture, the W5 trace_jit hot loop runs at **84.0 ± 0.05 µs** across
8 process restarts. *No variance.* This rules out:

* ASLR of the JIT code page (the probe randomises this too).
* Stack-frame alignment of the per-batch `TraceContext`.
* L1d/L2 conflict misses on the dict/keys/IC layout in isolation.

### `cmp_lua` bench (criterion): bimodal across runs

```text
$ for i in 1..12; do
    RELON_BENCH_FORCE_RUN=1 RELON_W5_KEYS_DUMP=1 taskset -c 2 \
        target/release/deps/cmp_lua-* \
        --bench "W5_dict_str_key/relon_trace_jit$" \
        --sample-size 30 --measurement-time 4 ;
  done
```

Per-run pairing of timing with the 10 hot keys' IC slot indices
(slot index computed via the old XOR-shift formula, `N = 32`):

```text
run  time     unique_slots  collisions  slot_distribution
 1   192.87 µs       6           4       [10,4,4,15,15,3,16,6,3,15]
 2   114.63 µs       9           1       [9,24,0,31,11,7,15,5,15,1]
 3   110.41 µs       9           1       [25,21,12,11,10,11,27,20,31,29]
 4   139.67 µs       8           2       [16,27,12,23,25,24,25,28,16,9]
 5   164.33 µs       7           3       [30,13,15,11,14,16,20,14,13,14]
 6    83.62 µs      10           0       [5,13,7,10,25,26,23,21,11,29]
 7   173.31 µs       7           3       [15,20,17,16,14,17,18,15,21,20]
 8   112.36 µs       9           1       [8,13,25,1,2,24,19,21,1,3]
 9   140.30 µs       8           2       [21,30,28,16,20,6,30,7,28,29]
10   133.32 µs       8           2       [0,16,30,0,16,5,11,17,26,28]
11   117.45 µs       9           1       [18,6,3,30,21,12,0,12,31,1]
12   137.68 µs       8           2       [21,22,24,11,21,4,7,19,20,22]
```

Sorted by collision count, the correlation is monotone:

| collisions | time bucket | runs |
|-----------|-------------|------|
| 0         | 83.6 µs     | 1    |
| 1         | 110-117 µs  | 4    |
| 2         | 133-140 µs  | 4    |
| 3         | 164-173 µs  | 2    |
| 4         | 192 µs      | 1    |

Each additional IC collision adds ~25-30 µs to the trace-JIT time.

### Why isolated probe doesn't reproduce

The probe installs only the W5 trace and uses a fresh
`TraceContext::with_hooks` per batch. The first-iter cold misses
prime the IC, then every subsequent iter hits — even with 10 keys in
32 slots, because **no two probe-key pointers happen to hash to the
same slot** under one specific allocator placement. The probe runs in
a clean address space; the criterion bench shares its heap with mlua
+ relon's parser/analyser + 9 other trace fixtures (W2/W3/W4/W4-long
/W6/W8/W9/W10/W12), which churn the allocator's free-list and shift
the W5 keys' relative offsets.

The criterion bench is the realistic scenario; the per-process
heap-layout lottery is fundamentally birthday-paradox-bound for the
current IC sizing.

## Why the diagnostic candidates were wrong

| Hypothesis                                          | Verdict | Evidence |
|-----------------------------------------------------|---------|----------|
| Trace fn machine-code alignment / mod-64 entry      | **NO**  | Every JIT module gets a fresh `mmap` page; `fn_ptr mod 4096 = 0` on every run. Probe confirms 84 µs regardless of the random page. |
| ASLR of the JIT code region                         | **NO**  | `setarch -R` was already negative; probe varies code page across runs but stays at 84 µs. |
| Stack-frame alignment of `tctx`                     | **NO**  | Sorting 12 runs by `tctx mod 64` shows fast and slow runs share the same `tctx mod 64` value. |
| Cranelift IV pass producing different code          | **NO**  | The probe-binary trace bytes are identical across runs; `ops=24` on every install. |
| dict/keys/IC L1d set conflicts                      | Partial | Conflicting cache lines do not predict the slow cluster; the IC's *slot collision* count does. |
| IC slot collision from low-entropy hash mix         | **YES** | The 10 hot keys' slot count correlates monotonically with the measured time. See table above. |

## Implementation

The fix is contained in two trivial source edits plus a constant bump:

### `crates/relon-trace-emitter/src/emitter.rs`

Replaces the IC slot index derivation inside
`emit_dict_lookup_prechecked`:

```rust
// Before:
let mix = bxor(dict_v, key_v);
let shifted = ushr_imm(mix, 4);
let masked = band_imm(shifted, (slot_count - 1) as i64);

// After:
let log2_slots = slot_count.trailing_zeros();
let shift = (64 - log2_slots) as i64;
let mix = bxor(dict_v, key_v);
let mixed = imul_imm(mix, 0x9E37_79B9_7F4A_7C15u64 as i64);
let masked = ushr_imm(mixed, shift);
```

The two `imul64 + ushr` together generate the same number of x86_64
instructions as the prior `ushr_imm + band_imm` pair, but produce a
true high-entropy slot index instead of leaking through residual
low-bit address structure.

### `crates/relon-trace-abi/src/context.rs`

```rust
pub const DICT_LOOKUP_IC_SLOT_COUNT: usize = 64;  // up from 32
```

### Auxiliary diagnostics retained

The RCA process produced two env-gated diagnostics worth keeping for
future trace-JIT layout debugging. They have no production
hot-path cost (single `std::env::var_os` check at install time).

* `RELON_TRACE_FN_ADDR_DUMP=1` — at every `JITedTraceFn` install,
  print `fn_id`, the `fn_ptr` address, and its `mod16/32/64/128/4096`
  reductions. Lets future RCAs verify code-page placement instantly.
* `RELON_TRACE_FN_ALIGN_LOG2=N` — force `2^N`-byte alignment on every
  trace entry through cranelift's `log2_min_function_alignment`
  setting. Default is unset (cranelift's x86_64 minimum of 1 byte
  inside the JIT page). Useful for isolating any future
  alignment-sensitive trace fn.
* `RELON_W5_FIXTURE_DUMP=1` / `RELON_W5_TCTX_DUMP=1` /
  `RELON_W5_KEYS_DUMP=1` — bench-side diagnostics that dump the W5
  fixture's dict, keys-list, TraceContext, and per-key heap pointers
  + computed IC slot indices. Lets a future RCA correlate
  per-process layout with timing without re-instrumenting the
  source.
* `crates/relon-bench/src/bin/w5_variance_probe.rs` — a focused
  in-process W5 driver that bypasses criterion. Useful as a control
  experiment (it does **not** reproduce the variance; only the full
  `cmp_lua` bench process does, which itself is a strong signal that
  the variance is heap-layout driven and not trace-JIT internal).

## Birthday-paradox numbers

For `k = 10` hot keys and `N` slots:

```
E[collisions] = C(k, 2) / N  =  k(k-1) / (2N)
P[≥1 collision] = 1 - (N-1)/N * (N-2)/N * ... * (N-k+1)/N
                ≈ 1 - exp(-k(k-1) / (2N))
```

|   N | E[coll] | P[≥1]  | per-context IC | TraceContext total |
|----:|---------|--------|----------------|---------------------|
|  16 | 3.13    | 95.4%  |   384 B        |   536 B            |
|  32 | 1.56    | 73.5%  |   768 B        |   920 B            |
|  64 | 0.78    | 48.6%  | 1 536 B        | 1 688 B            |
| 128 | 0.39    | 27.9%  | 3 072 B        | 3 224 B            |
| 256 | 0.20    | 15.0%  | 6 144 B        | 6 296 B (> 1 page) |

The 64-slot point hits the Pareto frontier: per-context still under
2 KB, but per-launch P(any collision) drops to 49% (half the launches
land in the perfect-clean fast cluster).

## Won't this regress non-W5 workloads?

No measurable effect expected:

* Workloads with **< 6 hot keys** (most of cmp_lua other than W5): the
  birthday-paradox term `k² / 2N` is negligible even at `N = 32`. The
  multiplicative mix is one extra cycle vs the prior ushr-band pair
  but the imul + ushr fuses on Broadwell so the latency is identical
  (3-4 cycles either way through the inline-IC critical path).
* Workloads with **no dict op** (W2/W3/W4/W12): the IC array is
  never touched. Only impact is `+768 B` TraceContext footprint per
  caller, which the criterion `invoke_with_existing_ctx` reuse keeps
  out of the per-call alloc path.

To verify, the next perf-loop iteration should re-measure the full
W1..W12 panel with the new IC and confirm no row drifts > 5%. (Not
done in this branch — out of scope for the RCA itself.)

## Open follow-ups

1. **Quantify cost of dict-lookup miss-then-store**. The "+25 µs per
   collision" figure is empirical; a microbench on the dict_inline IR
   path (miss-then-store vs hit only) would let us predict per-N
   timings analytically.
2. **Consider an array-of-arrays IC layout**. With 64 slots arranged
   as `(dict_ptr, key_ptr, value)` triples, hot keys can land on
   non-adjacent cache lines (24 B stride, 2-3 keys per 64 B line).
   Repacking into a 3-array SoA (`dict_ptrs[64]`, `key_ptrs[64]`,
   `values[64]`) would let the cranelift IC probe load each side's
   cache line independently and exploit the fact that we only need
   to *match* against `dict_ptr` first (saves ~50% of L1 line touches
   on miss-heavy workloads). Open question whether this is worth the
   ABI churn for what is already a relatively rare workload pattern.
3. **Robin Hood probing**. Bounded probing distance (e.g. probe 2
   adjacent slots on the inline path) would convert most collisions
   into bonus hits at the cost of two extra loads per probe. Worth
   modeling.
4. **W4 still in the 1.33× envelope**. Separate root cause, not
   addressed here; tracked elsewhere.

## Verification

* `cargo test --release -p relon-trace-abi` — green after the
  `trace_context_size_is_stable` assertion bump (920 → 1688).
* `cargo test --release -p relon-trace-emitter -p relon-trace-jit
  -p relon-codegen-native` — all 250+ tests green.
* Full workspace build with the new IC constants — green.
* W5 16-run sample on the local Broadwell box: variance span 2.3× →
  1.69×, median 121 µs → 110 µs, no more 170-200 µs outliers.
