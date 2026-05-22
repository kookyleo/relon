# Review improvement #171 — cache HMAC fail-closed (2026-05-22)

## Finding

Object-cache integration accepted `ensure_key() = Err` by falling back
to a no-HMAC write paired with `IntegrityMode::TrustOnWrite` reads — a
local attacker who could write `cache_dir` could drop an unauthenticated
`.relon-native-v1` blob and have it dlopen'd into the host. The schema
sidecar was even weaker: a self-attesting SHA-256 trailer (`schema_cache.rs`
v1) that any file-writer could trivially recompute, while
`evaluator.rs::from_cache_dir` transmuted the resulting `dlsym` pointers
into typed ABI function pointers (`run_main` + closure table). Together
the two bypasses gave the cache triple zero authentication on key-less
hosts and let an attacker swap the schema sidecar even on keyed hosts.

## Choice

**Option A (main) + Option B (sidecar sub-fix)** per the brief.

- Option A: refuse cache writes and loads entirely when
  `relon_object_cache::ensure_key()` fails. No-key hosts pay a cold
  start on every call; this is acceptable since the cache is a
  best-effort optimisation, not a correctness requirement.
- Option B: HMAC-seal the schema sidecar binding it to the object
  body's SHA-256 + the source key + the entry shape/arity. v1 sidecars
  are rejected by version mismatch so any stale file is regenerated.

## Implementation points

- `relon-object-cache`: new `IntegrityMode::HmacRequired` variant +
  `CacheError::HmacKeyRequired`. The storage `load` enforces
  `hmac_key.is_some()` up front so the integrity guarantee cannot be
  silently downgraded at the storage layer either. `TrustOnWrite`
  retained for legacy tests; new production callers use the explicit
  mode.
- `object_cache_integration.rs`: `try_store_to_cache` resolves the
  HMAC key first; failure short-circuits with a warn and writes
  nothing. On success it surfaces the linked ET_DYN's SHA-256 via
  the new `StoredObject` so the schema layer can bind. `try_load_from_cache`
  refuses with a warn on key failure and routes the load through
  `IntegrityMode::HmacRequired`. The returned `LoadedCache` carries
  `object_sha256` + `hmac_key` so the schema-cache loader uses
  exactly the same per-installation key as the object-cache layer.
- `schema_cache.rs` v2: trailing 32-byte HMAC over
  `b"RLSC-v2" || source_sha256 || object_sha256 || entry_shape || entry_arity || body`.
  `serialize` / `deserialize` take the bindings explicitly; v1 files
  return a version-mismatch error so the next cold start regenerates
  the triple.
- `evaluator.rs`: `write_cache_pair_best_effort` writes object + IR
  first, gets back `StoredObject.object_sha256`, then seals the
  sidecar; mid-write key failure invalidates the just-written triple.
  `from_cache_dir` threads `loaded.object_sha256` + `loaded.hmac_key`
  into the sidecar `deserialize` call.

## Tests (4 new regression surfaces, all passing)

- `cache_hmac_absence::cache_writes_are_refused_when_hmac_key_unavailable`:
  pins `XDG_DATA_HOME` to a 0o500 dir so `ensure_key` fails; asserts
  no object / IR / schema file is created and `from_cache_dir` returns
  None. Lives in its own test binary so the env mutation cannot race
  other tests in the suite.
- `object_cache_integration::tampered_schema_sidecar_invalidates_triple`:
  flips a body byte inside the HMAC-sealed sidecar; load path
  invalidates the triple and removes both files.
- `object_cache_integration::schema_sidecar_bound_to_object_hash`:
  unit-level proof that the HMAC rejects a swapped `object_sha256`.
- `hmac_required_mode` (object-cache crate, 3 sub-tests): explicit
  `HmacKeyRequired` error on `None` key; happy-path with key; HMAC
  catches in-place body tamper end-to-end. Plus 4 additional inline
  tests in `schema_cache::tests` covering tag corruption, mismatched
  object hash, mismatched source hash, and v1 layout rejection.

## Gate

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D warnings`,
and `cargo test --workspace` all clean. `cargo check --target
wasm32-unknown-unknown -p relon-wasm` clean. Workspace test count
**2315 passed / 0 failed / 6 ignored** (baseline 2306 + 9 new regression
tests across cache HMAC absence, sidecar tamper, and object hash
binding).

## Commits

- `fb52c39 refactor(object-cache): add HmacRequired integrity mode`
- `07d8697 fix(codegen-native): refuse cache load/write when HMAC key absent`
- `73c4d84 fix(codegen-native): schema sidecar HMAC binds object_hash + entry_shape`
- `01eca72 test(codegen-native): cache HMAC absence + sidecar tamper regression`
