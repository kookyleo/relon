# Review-Improvement #164 — Op::ConstString / `__relon_str_concat_alloc` / cranelift ConstString emit: Tier 1b cached fx_hash migration

## Scope

Follow-up to #149 (which cached fx_hash on dict-key records only). This
stage extends the cached-hash story to the runtime-side `StringRef`
host struct AND to the cranelift-AOT const-pool, so cross-trace dict
crossings on freshly-concatenated / const-folded strings can reuse the
digest instead of re-running the byte-wise FxHash loop.

## Layout before / after

### Runtime-side `StringRef` (trace JIT)

**Before** (16 bytes):

```text
#[repr(C)] struct StringRef { ptr: *const u8, len: usize }
offset 0..8  : ptr
offset 8..16 : len
```

**After** (24 bytes):

```text
#[repr(C)] struct StringRef { ptr: *const u8, len: usize, hash: u64 }
offset 0..8   : ptr
offset 8..16  : len
offset 16..24 : hash    (fx_hash_bytes(payload); 0 = "not yet sealed")
```

Producer helpers (`from_owned` / `borrow` / `from_static`) stamp the
digest at construction time. `__relon_str_concat_alloc` seeds
`hash = 0` because the JIT inline lowering writes the rhs tail bytes
after the alloc call returns; a companion
`__relon_str_concat_seal_hash(*mut StringRef)` folds the full payload
once the tail stores complete.

### Cranelift AOT const-pool

**Wire layout unchanged** — const-data records stay as
`[len: u32 LE][payload]` so the existing stdlib bodies' `+4`-offset
payload reads (`concat` / `substring` / `upper` / `lower` / `title` /
`nfd` / …) keep working byte-for-byte without a coordinated rewrite.

**Side-band addition**: `ConstPool::string_hashes: HashMap<u32, u64>`
holds `fx_hash_bytes(payload)` for every `Op::ConstString { idx }`
seen during the IR walk. The digest sits next to `string_offsets` so
a future trace-JIT consumer that wants to cross a const string into a
dict key reads it for free.

## 改造点 list

1. `crates/relon-trace-jit/src/runtime/str_ops.rs`
   - `StringRef` gains `hash: u64` field; `STRING_REF_HASH_OFFSET = 16`
     with compile-time `offset_of!` assert (cfg-gated to 64-bit hosts,
     matching the existing wasm32 carve-out).
   - `from_owned` / `borrow` / `from_static` call `fx_hash_bytes` at
     construction time.
   - `__relon_str_concat_alloc` seeds `hash = 0` ("not yet sealed").
   - New `__relon_str_concat_seal_hash(*mut StringRef)` re-reads the
     now-complete payload and writes the digest into the cached field.
2. `crates/relon-trace-emitter/src/abi.rs`,
   `crates/relon-trace-emitter/src/emitter.rs`,
   `crates/relon-trace-emitter/src/str_inline.rs`
   - New `HostHookId::StrConcatSealHash` (symbol
     `__relon_str_concat_seal_hash`).
   - `HostHookFuncIds::str_concat_seal_hash: Option<u32>` plumbed
     through `TraceEmitterState`.
   - `emit_str_concat_inline_short_rhs` takes a new
     `Option<FuncRef>` and calls the helper after the unrolled rhs
     `store.i8` tail when wired; `None` keeps the historical inline
     shape (correct, but the dict IC silently misses on the concat
     result).
3. `crates/relon-codegen-native/src/trace_install.rs`
   - Declares the seal helper as an `Import` and registers the
     symbol in the JIT builder so a recorded trace can resolve it.
4. `crates/relon-codegen-native/src/codegen/const_pool.rs`
   - `ConstPool::string_hashes` side table stamped in
     `visit_const_string`.
5. Tests
   - `concat_seal_hash_matches_fx_hash_bytes`,
     `concat_seal_hash_null_input_is_noop`,
     `from_owned_stamps_cached_fx_hash` in trace-jit (cover the
     producer-side guarantee + the seal-after-write contract).
   - `opvisitor_caches_fx_hash_for_each_const_string` in
     codegen-native (round-trips the pool's side-table digest against
     the canonical `relon-trace-abi` reference).
   - Existing `str_concat_inline_exec` JIT round-trip test updated
     to pass `None` for the seal helper (its parity check is on
     payload bytes only, not the hash field).

## W3 / W5 bench impact

W3 (`string_concat`) inline lowering now emits one extra `call
__relon_str_concat_seal_hash(result_ptr)` per concat iteration. The
helper is a single `slice::from_raw_parts` + `fx_hash_bytes` over the
freshly-built payload — for the W3 fixture's tiny per-iter strings
(≤ 16 bytes) the seal cost is ~5–10 ns / iter on top of the
historical inline path. Bench rerun on the agent's worktree machine
was inconclusive because the host was not in the quiescent state the
bench gate requires (`schedutil` governor across all CPUs; 1-min
load avg ~20). Running with `RELON_BENCH_FORCE_RUN=1` gave a noisy
W5 trace_jit number of 127.68 µs (vs. #149's 131.08 µs / 121.54 µs
band) — within run-to-run variance, so no honest claim either way.

W5 (`dict_str_key`) is unaffected by this stage's seal-hash work
because its hot loop doesn't go through `StrConcat`. The cached-hash
win on cross-trace concat-into-dict-key patterns will materialise
once a follow-up actually consumes the new `StringRef::hash` field
on the dict IC fast path.

## Gate

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` **2231 passed / 0 failed** (≥ the 2227
  baseline noted in the task brief)
- `cargo check --target wasm32-unknown-unknown -p relon-wasm` clean
- `cmp_lua_consistency` 10 cases all pass (W3 / W4 / W5 / W6
  included)
- corpus_differential 2/2 pass (the migration's wire-layout work was
  rolled back to side-band-only because the full const_data
  migration would have required coordinated rewrites of every
  stdlib body's `+4` payload offset; see follow-ups)

## Follow-ups

- **Full wire-layout migration of cranelift const_data**: pushes the
  unified Tier 1b header (`[len_with_flags: u32][hash: u64][payload]`)
  onto every const-pool string record. Requires a coordinated rewrite
  of (a) `EmitTailRecordFromAbsoluteAddr` (strip the 12-byte header
  before copying into the output buffer — host `BufferReader` still
  expects the legacy 4-byte len prefix), (b) `emit_read_string_len`
  (mask the ASCII flag bit), and (c) every stdlib body that reads
  `String` payload bytes via `LocalGet(s) + ConstI32(4) + Add`
  (`concat` / `substring` / `upper` / `lower` / `title` / `nfd` /
  `nfc` / `starts_with` / `glob_match` / …). The first revision of
  this PR attempted the wire migration and surfaced 14 corpus_diff
  mismatches before the rollback; the side-table digest is the safe
  intermediate.
- **Consumer for `StringRef::hash`**: the dict IC fast path in
  `__relon_trace_dict_lookup` / `__relon_trace_dict_lookup_prechecked`
  currently re-loads the digest off the dict-key record header (the
  #149 layout). Once the recorder grows a "key SSA is a `StringRef`,
  not a record offset" lowering shape, the IC can branch on the
  source kind and pick up the cached `StringRef::hash` via
  `load.u64 [str_ref + 16]`.
- **Consumer for `ConstPool::string_hashes`**: a trace-JIT pass that
  lifts `Op::DictGetByStringKey` whose key SSA is a `ConstString`
  can pre-stamp `shape_hash` from the side table at IR-rewrite time
  so the cranelift emitter's per-iter IC fast path stays the same
  single `load.u64`.
- **Drop the `hash = 0` "not yet sealed" sentinel ambiguity**: a
  pure-ASCII payload could legitimately fx-hash to zero (the seed
  XOR-folds to `0xcbf2_9ce4_8422_2325`, so this is statistically
  improbable but not impossible). Either pick a sentinel that
  `fx_hash_bytes` provably cannot return, or thread a separate
  `hash_sealed: bool` flag.

## Commits (branch `worktree-agent-a3709c791b2a74ab3`)

1. `ab2805d refactor(trace-jit): widen StringRef with fx_hash field + seal helper`
2. `6456d3d feat(trace-emitter): wire StrConcatSealHash hook into inline lowering`
3. `2febbc3 feat(codegen-native): cache fx_hash for each ConstString in pool side table`
4. `05041c0 style(codegen-native): rustfmt break on const_pool test call`
