# Review-Improvement #169 — Op::ConstString wire-layout full migration: audit + blocker analysis

## Scope

Follow-up to #149 (dict-key 12-byte header) and #164 (side-table
`fx_hash` cache; explicit follow-up section called out this exact
migration). Goal: collapse the cranelift const-pool `ConstString` wire
record from `[len: u32 LE][payload]` (4-byte header) to the unified
`[len_with_ascii_flag: u32 LE][hash: u64 LE][payload]` (12-byte
header) used today only on trace-JIT dict-key records, so producers /
consumers across const_data, host buffer protocol, codegen emit, and
every stdlib body share one canonical layout.

This stage stops at audit + blocker analysis: **no code change**
landed. The full migration is bit-level atomic — partial coverage
silently corrupts strings — and the per-site classification work below
is the gating prerequisite for the next coordinated wave.

## Baseline (worktree HEAD `cdf9751`, branch `worktree-agent-ab3aa3a38b66546f4`)

```
$ cargo test -p relon-test-harness --test corpus_differential
Differential corpus: 60 cases / 55 match_ok / 4 match_trap /
  1 cranelift_unsupported / 0 tree_walk_missing / 0 mismatch
```

`#164` recorded that an early attempt to flip the wire format in
isolation surfaced **14 corpus_diff mismatches**, because every
stdlib body still indexed the payload at `s + 4` while the producer
side had moved the payload to `s + 12`. The side-table digest in
`ConstPool::string_hashes` was the safe roll-back.

## Audit — every site that hard-codes the 4-byte header

The wire-layout flip cascades through six distinct surfaces. Each
must move in the same commit (or behind a feature flag) or the
intermediate revisions silently corrupt strings.

### 1. Producer: const-pool record emit
`crates/relon-codegen-native/src/codegen/const_pool.rs:136-157`
(`visit_const_string`) writes `[len: u32 LE][payload]`. Must become
`[len_with_ascii_flag: u32 LE][hash: u64 LE][payload]`, reusing
`relon_trace_abi::hash::{is_ascii_bytes, fx_hash_bytes,
STRING_RECORD_ASCII_FLAG_BIT, STRING_RECORD_PAYLOAD_OFFSET}`. The
side-table `string_hashes` becomes redundant and should be removed
in the same commit.

### 2. Producer: host buffer-protocol writer
`crates/relon-eval-api/src/buffer.rs:190-214` (`write_string`) calls
`append_tail_record(4, len, value.as_bytes())` which writes 4-byte
header + payload. The 12-byte variant needs an
`append_tail_string_record` helper that also emits the cached hash
and ASCII-flag bit; the public `write_string` API stays unchanged.

### 3. Consumer: host buffer-protocol reader
`crates/relon-eval-api/src/buffer.rs:1104-1132` (`read_string`) calls
`decode_pointer_header(field_name, ptr_offset, 0)` which returns
`(len, record_start + 4)`. Needs a String-specific variant that
masks the ASCII flag (`len & STRING_RECORD_LEN_MASK`) and skips the
12-byte header. `decode_pointer_header` is also used by `read_list_*`
(line 1153, 1208, 1260, 1306, 1400) so cannot be widened in place
without breaking list-record reads. Split into
`decode_string_pointer_header` (12 bytes, mask flag) vs
`decode_list_pointer_header` (4 bytes, no mask).

### 4. Consumer: cranelift `emit_read_string_len`
`crates/relon-codegen-native/src/codegen/field.rs:344-375`. Currently
`load.i32 [ptr]` then widens to i64. Must become `load.u32 [ptr];
band(v, 0x7FFFFFFF); uextend.i64`. The bounds-check `ptr + 4 <=
arena_len` is still correct (the header is 12 bytes but the load
only reads the first 4).

### 5. Consumer: cranelift `emit_tail_record_from_absolute`
`crates/relon-codegen-native/src/codegen/record.rs:44-100`. The
`IrType::String` arm computes `record_size = len_i32 + 4` from the
loaded header. Must become `record_size = (len_i32 &
0x7FFFFFFF) + 12`. The memcpy already operates on `src_abs +
record_size` and dest tail-cursor, so once `record_size` and `len`
match the 12-byte header convention the strip-header copy is
correct.

### 6. Consumers: stdlib bodies (53 hand-coded `Op::ConstI32(4)`
   sites across `defs.rs`, `case_fold.rs`, `normalization.rs`)

Per-file classification of every `Op::ConstI32(4)` literal in
`crates/relon-ir/src/stdlib/`:

#### `defs.rs` (13 sites)
All are string-payload offsets that must flip 4 → 12:
- `concat_string_string_body`: lines 424 (header size for alloc),
  452, 455, 467, 475 (5 sites: alloc record_size, base+4 dest,
  a+4 src, base+4+len_a, b+4 src — all 4 references to payload
  start, the 424 is the header size for `record_size = len + 4`).
- `substring_string_body`: lines 681, 703, 706 (alloc header size,
  base+4 dest, s+4 src — all payload-related).
- `starts_with_string_body`: lines 821, 831 (s+4+i, p+4+i).
- `contains_string_body`: lines 1123, 1138 (nested: s+4+i+j, p+4+j).

The remaining `defs.rs` `ConstI32(4)` literals (lines starting at
1255 `4 + 7` for List<Int> alignment, etc.) are List<Int> /
List<Float> related — they stay at 4.

#### `case_fold.rs` (14 sites)
Mixed: must classify each.
- 607, 1587, 1873, 2289, 2509, 2711, 2946, 3015, 3075: every site
  fits one of three shapes — UTF-8 codepoint byte count
  (`CP_BYTES = 4`, stays 4), table-entry stride
  (`table_addr + 4 + mid * 8`, stays 4 — it's the table's own
  4-byte len header), or string payload offset (flip to 12).
  The `e2.push(tt(Op::ConstI32(4)))` at 607 sets `CP_BYTES = 4`
  for a 4-byte UTF-8 sequence — stays. The `table_addr + 4` shape
  at 1873/2509/3015 is binary-search table indexing — stays.
- 1123, 1291, 1297, 1468, 1473: `case_fold_body_inner_body` (the
  big upper/lower/title body). 1468/1473 build the codepoint
  scratch buffer `alloc_scratch_dyn(4 + s_len * 4)` — the 4 is
  the CP-buffer's own 4-byte len header (stays 4). 1123/1291/1297
  are string payload accesses (`s + 4 + i` shape, flip to 12).

#### `normalization.rs` (26 sites)
Mixed and densest — every site must be hand-classified. Quick
sample:
- 135, 322, 504: binary-search inside decomp / CCC /
  composition lookup tables — `table_addr + 4 + mid * stride`,
  the 4 is the lookup table's own 4-byte len header. Stays 4.
- 815, 831: CP-buffer alloc + payload writes (CP buffer has
  4-byte header; stays 4).
- 1079, 1297-1308: pool-base computation
  `pool_base = table_addr + 4 + index_count * 12`, the `+4` is
  the index table's header. The `tt(Op::ConstI32(8))` at 1308
  is "header + pool_count" — stays 8 (CP-pool layout, not String).
- 1355, 1403, 1439, 1445, 1533, 1539, 1631, 1639, 1651, 1657: all
  CP-buffer reads (`cp_base + 4 + k*4` for u32 element access),
  4 is CP-buffer header. Stays 4.
- 1863, 1869, 1909, 1915, 2259, 2265, 2313, 2319, 2477, 2483,
  2632, 2681, 2696: mix of CP-buffer offsets and string payload
  offsets — needs per-line read. **Estimated half are CP-buffer
  (stay 4), half are string payload (flip to 12).**

### 7. Wasm backend (not in cranelift-native path)
`crates/relon-wasm` and the legacy wasm-AOT pipeline read / write
strings via the same Buffer protocol. If `relon-eval-api::buffer`
changes the wire format, the wasm side observes the same flip.
Need to confirm wasm runtime helpers (if any) that decode strings
also mask the ASCII flag bit before reading length.

### 8. ET_REL object-cache compatibility
`crates/relon-object-cache` serialises the cranelift `CompiledModule`
including `const_data` bytes. Existing on-disk caches built with the
4-byte header would silently mis-decode after the flip. Either bump
the cache version (`CACHE_FORMAT_VERSION`) to invalidate old
artifacts, or add a per-record schema-version byte. **No test
currently covers cache round-trip across the wire-layout change.**

## Blockers

1. **Per-site classification of all 26 `normalization.rs` sites**.
   The CP-buffer (`[len: u32][u32 codepoints...]`) and the String
   record (`[len: u32][utf8 bytes]`) share the literal `4` for
   different reasons — payload start vs codepoint stride header.
   Mis-classifying a single CP-buffer site as "string payload" and
   bumping it to 12 silently overreads into the next u32 codepoint
   slot, producing garbage normalized output that the corpus_diff
   harness sees as `MatchTrap` (length mismatch) or `MatchOk` with
   wrong bytes. Same risk on the reverse direction.

2. **No per-stdlib-fn micro-test for the wire layout**. The
   existing `corpus_differential` is the only gate — and it only
   asserts tree-walk vs cranelift agree on the answer, not that
   the cranelift output's wire bytes match a reference shape.
   A `wire_format_smoke` test that asserts the first 12 bytes of
   every `ConstString` record are `[len_with_flag][hash]` would
   catch a mis-classified site immediately.

3. **Cache-version bump policy unclear**. The follow-up question
   "should existing ET_REL caches built against the 4-byte header
   be silently invalidated, or migrated in place?" is not
   answered by any existing ADR. Defaulting to invalidation
   (bump `CACHE_FORMAT_VERSION`) is the safe path; needs explicit
   sign-off so the next CLI release doesn't surprise users with a
   one-time cold-start regression.

4. **`emit_tail_record_from_absolute` ListString / ListSchema**.
   These variants currently return a `CraneliftError::Codegen`
   ("pointer-array not yet supported"). The List<String> case
   becomes interesting because every per-element entry is itself
   a String record — needs to emit 12-byte headers per entry.
   Currently not exercised, but the next time someone adds
   `List<String>` to `EmitTailRecordFromAbsoluteAddr`, they
   inherit the wire format chosen here.

5. **Wasm32 backend audit not performed in this pass**. Reading
   `crates/relon-wasm` to enumerate string-decode call sites is a
   prerequisite to a coordinated flip; left as out-of-scope for
   the audit.

## Recommended next-wave commit plan (NOT executed)

The order matters — each commit must compile + pass
`corpus_differential` standalone, so the producer / consumer pairs
move together within a single commit.

1. `refactor(ir): STRING_RECORD_PAYLOAD_OFFSET constant + helper
   tt(Op::ConstI32(STRING_RECORD_PAYLOAD_OFFSET))` — value stays
   at `4`; no behaviour change. Replaces the **classified-as-
   string-payload** `Op::ConstI32(4)` literals in stdlib (per the
   per-file lists above). Site count: 13 in `defs.rs`, ~5 in
   `case_fold.rs`, ~13 in `normalization.rs`. Each line audited
   individually.

2. `refactor(eval-api): split decode_string_pointer_header from
   decode_list_pointer_header` — pure refactor, no wire change.
   Unblocks the asymmetric header sizes in commit 5.

3. `refactor(codegen-native): split emit_read_string_len vs
   emit_read_list_len` — same shape as commit 2; today they
   share the codegen but post-flip the String path masks the
   ASCII bit.

4. `test(wire-format): add wire_format_smoke that asserts every
   ConstString record header is exactly 4 bytes today` — locks
   the current invariant down so commit 5's flip can't silently
   leave a 4-byte producer in place.

5. **The atomic flip**:
   `feat(wire-format): widen ConstString header to 12 bytes`.
   Bumps `STRING_RECORD_PAYLOAD_OFFSET` from 4 → 12, switches
   `const_pool.rs` / `buffer.rs:write_string` / `record.rs`
   `EmitTailRecordFromAbsoluteAddr String arm` / `field.rs`
   `emit_read_string_len` / `buffer.rs:read_string` all in
   lockstep. Updates the `wire_format_smoke` test to assert
   12 bytes. Drops `ConstPool::string_hashes` side table.
   `corpus_differential` should land at 0 mismatch in this same
   commit; if any of the 26 `normalization.rs` sites surface
   as `MatchTrap`, the commit is reverted and the offending
   site re-classified.

6. `refactor(object-cache): bump CACHE_FORMAT_VERSION` — picks up
   the wire change so stale `.relon-cache/` entries are
   invalidated rather than mis-decoded.

## Gate (audit-only)

- `cargo fmt --all --check` clean (no code change)
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` baseline preserved
- `cargo test -p relon-test-harness --test corpus_differential`:
  60 cases / 55 match_ok / 4 match_trap / 1 unsupported / **0
  mismatch** (baseline maintained — no code change in this stage)

## Bench impact

Not measured. Audit-only stage. Once commit 5 lands, expected
deltas:

- **W3 `string_concat`**: neutral. Concat output already wires
  through `__relon_str_concat_alloc` which uses the runtime
  `StringRef` (16-byte field; out-of-scope for the const-pool
  wire). The producer/consumer cost per byte is unchanged.
- **W5 `dict_str_key`**: neutral on hot path. Dict-key records
  already carry the 12-byte header; the lookup never crosses a
  const-pool string. The win will materialise only when a
  follow-up lifts `Op::DictGetByStringKey` whose key SSA is a
  `ConstString` and pre-stamps `shape_hash` from the const-pool
  digest (currently in the `string_hashes` side table; post-flip
  in the wire header itself).
- **W6 `string_concat_then_dict_lookup`**: small win when the
  concat result is the dict key — the dict IC can pull the
  cached hash off the result record header instead of re-hashing.
  Magnitude depends on payload length; rough estimate
  ~5–10 ns / op for ≤ 16-byte keys.

## Branch + commit SHA

- branch: `worktree-agent-ab3aa3a38b66546f4`
- HEAD before this stage: `cdf9751` (workspace tip)
- This stage adds the audit report only; commit SHA noted on land.

## Follow-ups

- **Run commit plan steps 1-6 in a follow-up phase**, ordered as
  above. Step 1 (`STRING_RECORD_PAYLOAD_OFFSET` constant introduction
  with value still 4) is the first low-risk lever; landing it
  separately confirms the audit's per-site classification holds
  before the atomic flip in step 5.
- **Wasm backend audit**: enumerate every string-decode call
  site in `crates/relon-wasm` and the wasm-AOT runtime helpers;
  add to commit 5's lockstep set.
- **Cache-version policy ADR**: decide invalidation vs migration
  for the ET_REL cache.
