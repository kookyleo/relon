# Review #175 P2 — trace runtime fixture-only assumption leak fix

Worktree: `worktree-agent-a14de1542df39177d`
Base: `fc2611b` (local main HEAD)
Commits:
- `529d95f` fix(trace-jit): dict_lookup_v2 compares key payload after hash match
- `ccc615b` refactor(trace-jit): thread-local arena reclaims StringRef allocs

## Two sub-tasks

### (b) `dict_lookup` hash-collision silent corruption

The v1 dict layout stored only `(key_hash, value)` per entry and
returned the first FxHash64 match without verifying the key payload.
A future host plumbing a dict whose key set contains a payload-
distinct entry that collides on the 64-bit hash would silently get
the wrong value back. The hot path inside the cranelift emitter
already inlines the helper body and stays on the v1 layout, so the
fix preserves the v1 helpers' surface and signatures (callers in
`relon-trace-emitter`, the benches, and the unit tests keep
working) but adds clear docs spelling out the bench-fixture-only
correctness contract.

A new **v2 family** lives alongside:
- `build_dict_record_v2(shape, &[(payload, value)])` appends each
  key payload to the record tail and stores `(key_hash, key_off,
  key_len, value)` per entry (24-byte stride).
- `__relon_trace_dict_lookup_v2(dict_ptr, record_len, key_ptr,
  shape_hash, ctx)` performs the v1 `(shape compare + key hash
  scan)` then **bounds-checks** the stored payload range against
  `record_len` and `memcmp`s the stored bytes against the looked-up
  payload before returning. Hash collisions therefore degrade to
  one extra `memcmp` per collision instead of silent corruption.
- `__relon_trace_dict_lookup_prechecked_v2` mirrors the prechecked
  variant.

### (a) `StringRef` allocation arena

Every `str_ops` shim handing back a `*const StringRef` used to leak
its allocation (`Box::into_raw` for the header, `mem::forget`-style
on the payload, raw `alloc(Layout)` for the single-block
`concat_alloc` / `concat_n_alloc` shape). A new thread-local
`TRACE_STRING_ARENA: RefCell<Vec<TraceStringAlloc>>` captures the
allocation in one of three reclaim-shape enum variants
(`BoxedHeader` / `OwnedHeaderAndPayload` / `SingleBlock`); the host
calls `reclaim_trace_strings()` at trace exit / deopt to drop the
chain via the matching `dealloc` / `Box::from_raw` path. The
historical leak semantics survive by simply not calling reclaim.

## Bench delta (W5 / W6 / W3)

Hardware non-quiescent (`load1≈4`, `governor=schedutil`,
RELON_BENCH_FORCE_RUN); numbers are noisy, but **no regression**
detected on the trace-JIT hot paths.

| Bench | Base (fc2611b) | After (ccc615b) | Δ          |
|-------|----------------|------------------|------------|
| W5 trace_jit (`dict_str_key`) | 129.6 µs | 120.7–124.5 µs | -4 % (noise) |
| W6 trace_jit (`dict_num_key`) | 17.77 µs | 17.74–17.78 µs | +0 %         |
| W3 trace_jit (`str_concat`)   | 1.96 ms  | 1.94 ms          | -1 %         |

The v1 dict helper hot code is unchanged; the W5 / W6 deltas are
expected to be within criterion's significance band. W3 exercises
the arena `push` once per `__relon_str_concat_alloc` call; the
per-iter cost of pushing one `TraceStringAlloc::SingleBlock` onto a
thread-local Vec is well below the surrounding allocator cost (no
measurable regression).

## Hash-collision regression test

`dict_lookup_v2_hash_collision_returns_correct_value` forges a key
record whose **cached `hash` field is stamped with the FxHash of an
unrelated payload** (so `fx_hash_key_record` returns the colliding
digest) but whose payload bytes are distinct. The v1 helper would
return the colliding entry's value; the v2 helper rejects it with
`DICT_LOOKUP_DEOPT` after the payload `memcmp` fails. A second test
(`dict_lookup_v2_hash_collision_keeps_scanning_for_real_entry`)
hand-builds a record with two entries sharing the same `key_hash`
slot and verifies the v2 helper scans past the colliding-payload
entry to return the genuinely-matching one. Both regression cases
are mirrored on `__relon_trace_dict_lookup_prechecked_v2`.

## Leak-fix evidence

Four new tests under `runtime::str_ops::tests` confirm the arena
reclamation path:

- `arena_records_every_shim_allocation` walks every shim
  (`from_static`, `from_owned`, `__relon_str_concat`,
  `__relon_str_concat_alloc`, `__relon_str_concat_n_alloc`,
  `__relon_str_substring`) and asserts the arena counter grows on
  each call, then drops to zero after `reclaim_trace_strings()`.
- `reclaim_releases_owned_payload_buffers` allocates 256
  large-payload `from_owned` strings on a fresh thread, reclaims,
  and asserts the counter returns to zero (smoke test for the
  `OwnedHeaderAndPayload` reclaim — wrong `(ptr, len)` reconstruct
  would tripwire under miri / ASan).
- `reclaim_releases_single_block_concat_buffers` does the same for
  512 `concat_alloc` blocks (the W3 hot-loop shape) verifying the
  `SingleBlock` reclaim drains a long arena.
- `reclaim_is_idempotent_on_empty_arena` confirms double-reclaim /
  cold-thread reclaim is a no-op rather than a double-free.
- `reclaim_does_not_cross_thread_boundaries` verifies the thread-
  local arena is isolated per OS thread.

## Gate

```
cargo fmt --all -- --check         ✓ clean
cargo clippy --workspace --all-targets -- -D warnings   ✓ clean
cargo test --workspace             ✓ 2296 tests passing (baseline 2282 + 14 new)
cargo check -p relon-trace-jit --target wasm32-unknown-unknown   ✓ clean
```

(workspace-wide `cargo check --target wasm32-unknown-unknown` fails
on the pre-existing `region` crate dep, unrelated to this change —
same failure exists at base `fc2611b`.)

## Known follow-up (out of scope)

The cranelift emitter's `dict_inline::emit_dict_lookup_inline*` family
inlines the v1 hash-only scan directly into the trace IR. That
inline path inherits the v1 collision caveat and still needs a
separate emitter-side fix to either (a) route through the v2 layout
or (b) emit an inline `memcmp` after the hash hit. Scope of that
work is recorder + emitter + bench fixtures together; it is tracked
as a follow-up rather than rolled into this commit set.
