# v6 perf target — push from × 2 → × 1.5 across all 5 必过 dimensions

## Baseline (× 2 official target, met)

| Dim | Workload | Current | × 1.5 target | Gap |
|---|---|---:|---:|---:|
| D1 | W1 / W2 hot loop | (re-bench needed) | ≤ 1.5× | unknown |
| D2 | W11 cold start (lite) | × 1.59 | ≤ 1.5× | small (~6%) |
| D2 | W11 cold start (default) | × 1.63 | ≤ 1.5× | small (~8%) |
| D5 | W7 p99 tail | (re-bench needed) | ≤ 1.5× | unknown |
| D7 | W3 string concat | × 1.60 | ≤ 1.5× | small (~7%) |
| D7 | W4 string contains | × 1.66 | ≤ 1.5× | medium (~10%) |
| D8 | W5 dict_str_key | × 1.95 | ≤ 1.5× | **large (~23%)** |
| D8 | W6 dict_num_key | × 0.59 | ≤ 1.5× | **already done** |

## Honest expectation

W6 already under × 1.5. W3/W4/W11 within 10% of target — achievable
with focused tuning. W5 is the major gap; F-D8-E.2 (IC inline) + E.3
(LICM bounds) currently in flight estimate ~× 1.4-1.5 when both land.
D1/D5 need re-measurement before scoping work.

## Phase plan

### Phase A — in flight (commits f9c45ee..9906117 + agents a/b)

- **F-D8-E.1 TraceOp::Mod** — done (no W5 ratio change, but trace-IR
  gap closed + step_one dispatch bug fixed; later phases benefit)
- **F-D8-E.2 DictLookup IC inline** — agent `af046848ba74366a1`
  targeting W5 → × 1.4-1.5 via emit-time loop-invariant shape_hash
  hoist
- **F-D8-E.3 LICM ListGet bounds + DictLookup hoist** — agent
  `aedde2f74a3f6c924` extending licm.rs hoistable set
- **Mod overflow correctness** — fixed in `9906117` (independent fix,
  reviewer-flagged P2 P2)

### Phase B — D7 string tightening (after Phase A lands)

- **F-D7-E** `__relon_str_contains` needle=1 SIMD memchr — wasm v128 +
  native SSE memchr. Target W4 × 1.66 → ≈ × 1.3.
- **F-D7-F** Boyer-Moore for needle 2-16 — risk: byte-identical
  invariant under multi-byte UTF-8 needles. Decide go/no-go after
  F-D7-E lands.
- **F-D7-G** LICM string-trace probe hoist — when haystack is loop-
  invariant, hoist the StringRef payload load (already mostly via
  `HaystackHandle::Preloaded` in hand-built; recorder path needs it).
  Target W3 × 1.60 → × 1.4.

### Phase C — D2 cold-start tightening

- **F-D2-G** lazy stdlib body construction — only build the
  `StdlibFunction` entries that `stdlib_method_index` resolves for the
  script. Currently `builtin_stdlib()` returns the full bundle eagerly.
  Target W11 default × 1.63 → × 1.4.
- **F-D2-H** analyzer fast-path for trivial scalar `#main` — already
  partially via `is_trivial_scalar_main` (F-D2-default). Extend to
  skip carrier-injection pass entirely for the trivial shape.
  Target W11 lite × 1.59 → × 1.45.

### Phase D — D1 / D5 baseline + tightening

- **Re-bench D1 (W1/W2 hot loop)** quiescent — `scripts/
  bench_quiescence.sh` first. Get a real number. Decide work scope
  after.
- **Re-bench D5 (W7 p99 tail)** quiescent. Same.
- Likely levers if needed:
  - More aggressive LICM (covering Mod / Cmp / arithmetic chains)
  - Trace deopt-recovery fast-path (current deopt is O(snapshot_size))
  - Register hint / call-conv tuning (CallConv::Tail already tried)

### Phase E — final cross-dim re-bench + report

After A-D, run the full λ-2 12-workload matrix on a quiescent box +
compare to LuaJIT baseline. Update `relon-vs-luajit-final-report-
2026-05-19.md` with × 1.5 verdict per dimension.

## Risks + escape hatches

- **W4 multi-byte string contains (F-D7-F)** is the highest-risk
  lever; if Boyer-Moore breaks byte-identical, fall back to recorder-
  driven `__relon_str_contains` extern shim (current × 1.66) and call
  W4 closed at × 1.66 (still ≤ × 2).
- **W11 default-path** depends on cranelift-AOT cache hit rate; if
  cache invalidation is high in real workloads, the lazy stdlib lever
  may not show up in bench numbers — accept × 1.6 as floor.
- **D1/D5** unknown. If they're already ≤ × 1.5, declare them done at
  Phase D's baseline run; if they're > × 1.5, scope adds work.

## Stop conditions

Each phase ships independently; stop the × 1.5 push when any of:

1. All 7 (sub-)workloads land ≤ × 1.5 (success).
2. Three consecutive phases produce < 5% improvement on their target
   workload (diminishing returns).
3. A correctness regression surfaces and the fix moves a ratio above
   × 2 (revert the offender, ship at × 2 floor).

## Tracking

Task IDs:
- #105 F-D8-E.2 (in_progress)
- #106 F-D8-E.3 (in_progress)
- Future: #107 F-D7-E (needle=1 SIMD), #108 F-D7-F (Boyer-Moore),
  #109 F-D7-G (recorder string LICM), #110 F-D2-G (lazy stdlib),
  #111 F-D2-H (analyzer fast-path), #112 D1/D5 baseline.

Cron `c64ac57c` (3m) currently tracks Phase A. Replaced once Phase A
lands by a Phase B/C/D tracker.
