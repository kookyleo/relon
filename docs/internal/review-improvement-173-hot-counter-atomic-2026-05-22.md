# Review improvement #173 — hot-counter atomic table (2026-05-22)

## Finding

`relon-codegen-native::trace_install` held its global hot-counter
table as `UnsafeCell<[u32; MAX_FN_ID]>` with a hand-written
`unsafe impl Sync`. The on-file rationale claimed torn writes were
acceptable because at worst a race "delays a hot trigger by one
iteration". That reasoning is unsound: under the Rust memory model
a non-atomic write from JIT-emitted code racing a non-atomic read
from Rust (or another JIT-emitted write from a sibling thread) is
a data race, i.e. undefined behaviour — independent of the
observable counter value. The previous module doc-comment even
contradicted itself by describing the storage as "atomic" inside
`sandbox.rs`.

## Option A — AtomicU32 table

Storage switches to `[AtomicU32; MAX_FN_ID]` (initialised through
`[const { AtomicU32::new(0) }; MAX_FN_ID]`). The `unsafe impl Sync`
is gone — atomic arrays are naturally `Sync`. Layout / alignment
are identical to `u32`, so `hot_counters_base() -> *mut u32` stays
sound and the JIT continues to address slots as 4-byte integers.
Rust-side accessors (`hot_counter_peek` / `hot_counter_reset` /
`hot_counter_reset_all`) move to `AtomicU32::load` / `store` with
`Ordering::Relaxed`.

## Cranelift IR change

The prologue dropped its `load.i32 / iadd_imm / store.i32` triple
and now emits a single

```text
%old = atomic_rmw.i32 add trusted %slot, %one
%new = iadd_imm.i32 %old, 1
```

`atomic_rmw` returns the old value, so the new value is recovered
by an in-register `iadd_imm` for the threshold compare — no extra
atomic load is needed. Cranelift's `atomic_rmw` is sequentially
consistent, which is stronger than the `Relaxed` we logically
need, but the lowering is still a single locked instruction on
both target ISAs.

## Performance impact

On x86 the lowering is `LOCK XADD m32, r32` (one µop on Intel
since Skylake, ~1 cycle when uncontended). On aarch64 it's
`LDADDAL w0, w0, [x1]` with similar throughput. The pre-fix code
already issued three serially-dependent memory ops (load, add,
store); replacing them with a single locked RMW is net neutral on
warm-path latency and removes the bus-snooping pressure from
having the cache line bounce between cores under contention. No
measurable bench delta is expected.

## Tests

Added `hot_counter_multi_thread_no_lost_updates`: 8 worker threads
each `fetch_add(1)` the same slot 5_000 times after a release
barrier; the main thread asserts the slot equals
`THREADS * BUMPS_PER_THREAD`. Pre-fix this would observe lost
updates (and trigger the underlying UB); post-fix the value is
exact. Full gate: fmt --check, clippy -D warnings, cargo test
(2316 passed), wasm32 check — all green.

## Commits

- `fix(codegen-native): __relon_hot_counters uses AtomicU32 table`
- `test(codegen-native): multi-thread hot counter no race`
