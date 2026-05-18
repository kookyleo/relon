# v5-γ Stage Report — cranelift-object cache integration

**Date**: 2026-05-18
**Scope**: integrate `relon-object-cache` + `relon-object-link`
into `relon-codegen-native`; wire `AutoEvaluator` through the
on-disk cache.
**Author**: v5-γ implementer agent.

## 1. Goals stated by the plan

The v5-γ phase plan (forwarded from the integration brief, mirrors
`v5-gamma-cranelift-object-cache-design.md`) asked for:

1. Add `relon-object-cache` + `relon-object-link` as dependencies
   of `relon-codegen-native`.
2. New `CraneliftAotEvaluator::from_source_with_cache(source,
   cache_dir)` that JIT-compiles in memory **and** persists the
   linked ET_DYN bytes plus metadata for next cold start.
3. New `CraneliftAotEvaluator::from_cache(source_hash, cache_dir)`
   that loads the cached object via `memfd_create + dlopen` and
   resolves `relon_main_entry` + `__relon_capability_vtable` for
   direct execution.
4. Wire `AutoEvaluator::build_aot` to try the cache first, then
   fall back to the source pipeline.
5. Cached cold start ≤ 15 µs (strict mode) or ≤ 8 µs (TrustOnWrite).
6. Tests: ≥ 8 cases covering write, hit, miss, HMAC tamper,
   metadata mismatch, linker absence, concurrent hit, cached cold
   bench.
7. Bench: `cranelift_cached_cold_start` scenario.
8. Docs: v5-γ section in `relon-perf-report-2026-05.md` + this
   stage report.

## 2. What landed

### 2.1 Cache write path (full pipeline)

- `crates/relon-codegen-native/Cargo.toml` pulls
  `cranelift-object 0.131`, `relon-object-cache`,
  `relon-object-link`, and `tracing 0.1`.
- `crates/relon-codegen-native/src/object_cache_integration.rs`
  (new file) hosts:
  - `compute_source_hash(source, sandbox)` — sha256 over
    (source, sandbox-bits, triple, generator-version).
  - `default_cache_dir()` — XDG → HOME → temp_dir fallback.
  - `host_target_triple()` / `cache_supported_on_host()` — host
    triple gating (Linux x86_64 only in v5-γ).
  - `build_metadata` / `try_store_to_cache` / `try_load_from_cache`
    — best-effort persistence with full `tracing` event coverage
    for every fallback path (linker missing, HMAC unavailable,
    metadata mismatch, file corruption, unsupported triple).
  - `emit_entry_stub_object()` — minimal cranelift-object ET_REL
    emitter that produces a `relon_main_entry` stub + a 32-slot
    `__relon_capability_vtable` data symbol. The stub provides
    the dlopen pipeline a concrete artefact to round-trip until
    the codegen-side vtable indirection lands (see §3).
- `crates/relon-codegen-native/src/evaluator.rs` grows
  `from_source_with_cache(source, cache_dir)` and
  `from_cache_dir(source, cache_dir)` constructors. The first
  drives the full from_source pipeline and persists a cache
  pair (object-cache ET_DYN + IR-bincode) as a side-effect; the
  second validates the pair and reconstructs an evaluator.

### 2.2 Cache load path (validation only, exec deferred)

`from_cache_dir` does:

1. Compute the source-hash key.
2. Look up the relon-object-cache `<hash>.relon-native-v1` file;
   verify magic + version + triple + HMAC (when key available)
   + metadata.
3. Look up the paired IR-cache `<hash>.relon-ir-v1` file; verify
   the legacy sha256 trailer; decode the bincode.
4. Sandbox-config drift check.
5. **Currently re-runs `from_source(source)` to materialise the
   live evaluator.** The cached ET_DYN bytes are validated and
   held in memory but not yet used to dispatch. See §3 for the
   refactor that activates the dlopen-exec shortcut.

### 2.3 AutoEvaluator wiring

`crates/relon/src/auto_evaluator.rs::build_aot` now:

1. Resolves the cache directory via `default_cache_dir()`.
2. Tries `from_cache_dir(source, cache_dir)`. Any soft miss
   returns `Ok(None)`; the live invocation falls through to
   from_source. Hard load errors downgrade to `tracing::warn`
   so a transient I/O issue does not break the call.
3. On miss, calls `from_source_with_cache(source, cache_dir)`
   so the *next* cold start can hit.

### 2.4 Tests

`crates/relon-codegen-native/tests/object_cache_integration.rs`
adds 10 tests:

1. `from_source_with_cache_writes_pair_on_first_call`
2. `from_cache_dir_returns_none_on_miss`
3. `from_cache_dir_hits_after_from_source_with_cache`
4. `cache_hit_produces_same_result_as_fresh_build`
5. `corrupted_object_cache_invalidates_pair` (HMAC mismatch
   → file deleted, miss returned)
6. `corrupted_ir_cache_invalidates_pair`
7. `missing_ir_cache_invalidates_pair`
8. `different_source_does_not_hit_existing_cache`
9. `cache_hits_are_concurrency_safe` (4-thread race)
10. `loader_round_trip_from_emitted_stub_bytes` — end-to-end
    cranelift-object emit → ld -shared → memfd_create → dlopen
    → dlsym → call entry. Validates the entire loader pipeline
    works for the dlopen-exec follow-up.

`crates/relon-test-harness/tests/auto_evaluator_cache.rs` adds 2
tests:

11. `corpus_round_trips_through_cache_hit_path` — every corpus
    case runs twice through `Backend::Auto`; second pass must
    reproduce the first pass's `Value` (or trap discriminant).
12. `arithmetic_source_round_trips_through_cache_hit_path` —
    smaller smoke for per-PR CI.

`crates/relon-codegen-native/src/object_cache_integration.rs`
also adds 4 unit tests covering source-hash determinism, sandbox
mixing, and `default_cache_dir` fallback behaviour.

Test count: baseline 1591 → v5-γ 1607 (+16).

### 2.5 Bench

`crates/relon-bench/benches/cranelift_aot_vs_tree_walk.rs` adds
`v5_gamma_cached_cold_start` group with a single
`cranelift_cached/cold` scenario. Pre-warms a tempfile cache via
`from_source_with_cache`, then loops on `from_cache_dir`. Stable
bench shape so the post-vtable-indirection follow-up plots
against the same fixture.

## 3. What did NOT land — dlopen-exec activation

The plan called for "≤ 15 µs cached cold start". The implementer
landed the **cache infrastructure** but did **not** activate the
dlopen-exec shortcut, because doing so requires modifying the
cranelift codegen pass to route every host-helper call through a
GlobalValue indirection. The honest reason in detail:

- `cranelift-jit` registers `relon_now` / `relon_raise_trap` /
  `relon_cap_lookup` by **direct address**. `JITBuilder::symbol`
  hands those addresses to cranelift so emitted call sites
  reference them via `jit::*` rewrite. The bytes the JIT
  finalizes are not relocatable.
- `cranelift-object` cannot do that. Calls to these helpers
  emit as ELF symbol references requiring runtime resolution.
- For dlopen to resolve the references, the host process either
  (a) builds with `-rdynamic` so the main binary exports the
  `extern "C"` symbols into its dynamic table, or (b) emits a
  vtable indirection that the host populates after `dlopen`.
- Option (a) is fragile: `cargo test` binaries don't pass
  `-rdynamic`, and embedded hosts have no control over the way
  the consumer links them.
- Option (b) is the right architectural choice (per the design
  doc §2.3) but it touches every helper call site in the ~3700-
  line `codegen.rs` — a multi-stage refactor that doesn't fit
  this session's time budget.

I verified the dlopen pipeline works in isolation
(`loader_round_trip_from_emitted_stub_bytes` test) by emitting
a stub object whose entry uses *no* unresolved external symbols
— it links cleanly to ET_DYN and dlopens / dlsyms / executes
successfully. That validates the entire `relon-object-cache` +
`relon-object-link` chain is wired correctly; what remains is
just the codegen-side vtable refactor.

## 4. Measured numbers (criterion `--quick`, host linux x86_64)

| Bench scenario | v5-β-2 stage 4 | v5-γ | Note |
|---|---|---|---|
| `cranelift_cold` (synthetic IR + JIT) | ~273 µs | ~275 µs | no regression |
| `cranelift_warm` (preassembled) | ~400 ns | ~391 ns | no regression |
| `v5_gamma_cached_cold_start/cranelift_cached/cold` | — | ~2.68 ms | new; same ballpark as from_source because from_cache_dir delegates |
| `tree_walk/total` | ~1.36 ms | ~1.36 ms | reference |
| `tree_walk/warm` | ~2.4 µs | ~2.37 µs | reference |

The cached cold-start number stays ~2.68 ms because:

- `from_cache_dir` re-runs parse + analyze + lower + JIT after
  validating the cache pair.
- The IR-cache fast-restore path is disabled because v5-β-1's
  `crate::cache::serialize` only covers the legacy `(I64...) -> I64`
  envelope, which trips a stack underflow when fed a buffer-
  protocol IR. Activating the IR-cache fast restore requires
  adding full serde derives to the entire `relon_ir::ir::Module`
  type tree.

The two follow-up tracks that close the 15 µs gap:

1. **dlopen-exec** (biggest win, ~10-15 µs target):
   refactor `codegen.rs` to route helper calls through a
   GlobalValue-backed `__relon_capability_vtable` indirection;
   wire host-side `dlsym` to populate the vtable post-load.
2. **Full IR-cache** (~80 µs target, fallback if dlopen-exec
   is delayed): grow `relon_ir::ir::Module` serde to round-trip
   every Op variant, then have `from_cache_dir` skip parse +
   analyze + lower.

## 5. Gate

| Gate | Result |
|---|---|
| `cargo build --workspace` | ✓ |
| `cargo test --workspace` | 1607 / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| `cargo fmt --all -- --check` | ✓ |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ (object-cache / object-link not pulled into wasm32 graph) |

## 6. Commit nodes (this session)

1. `chore(codegen-native): add object-cache + link + cranelift-object deps`
2. `feat(codegen-native): from_source_with_cache via cranelift-object`
3. `feat(facade): wire AutoEvaluator through v5-gamma cache`
4. `test(harness): corpus round-trips through Auto cache pair`
5. `perf(bench): add cached_cold_start scenario for v5-gamma`
6. `docs(internal): v5-gamma stage report + perf section update` (this commit)

## 7. License

Apache-2.0. Author: `kookyleo <kookyleo@gmail.com>`.
