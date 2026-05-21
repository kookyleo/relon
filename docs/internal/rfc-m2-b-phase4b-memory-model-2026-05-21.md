# RFC — v6-δ M2-B Phase 4b — Bytecode VM Memory Model

Date: 2026-05-21
Status: planning + scaffold land
Owner: bytecode VM track
Worktree: `.claude/worktrees/agent-ade0586362ac835b5`
Predecessor: `docs/internal/review-improvement-141-bytecode-phase4a-2026-05-21.md`

## 1. Problem

Phase 4a wired the host-fn registry on `CapabilityVtable` and unlocked
the scalar `BcOp::CallNative` dispatch lane. The 4-way differential
harness still routes any list / dict / string source through
`BytecodeUnsupported` because the bytecode VM has no representation for
those shapes — `BcOp` carries only `u64` slots, and every `Op::Const*`
that lifts a heap value collapses it to its length (the M2-A
constant-fold trick). This RFC picks the memory model the bytecode VM
will use and lays out the phase-4b op-table expansion.

## 2. Memory model — option A / B / C

### Option A — reuse `relon_eval_api::Value`

Each operand-stack slot becomes a `Value` enum. Pros: zero design
work, full fidelity with the tree-walker, drop-in for the host-fn
encode lane. Cons:

- Every push / pop clones the enum (24-byte payload + 8-byte tag) →
  arith ops degrade to indirect-tag dispatch.
- `Value::List(Vec<Value>) / Value::Dict(BTreeMap<…>) /
  Value::String(Arc<str>)` carry their own heap pointers so list mutation
  re-borrows the Vec on every `ListPush`.
- Scalar lane regresses ~3-5× (matches the tree-walker but the M2-A
  benchmark numbers we have were taken against `u64` slots; we'd lose
  the floor we just established).

Verdict: reject for the dispatch loop; revisit for the host-fn
encode boundary only.

### Option B — handle-based + per-VM arena (recommended)

Operand-stack slot stays `u64`. List / dict / string values live in
type-specific arenas keyed by `u32` handles:

```
ListArena   slots: Vec<ListEntry>   handle = slot index
DictArena   slots: Vec<DictEntry>   handle = slot index
StringArena slots: Vec<StringEntry> handle = slot index
```

Each arena entry is an `Arc<T>` so `clone()` is a refcount bump; the
arena owns the slot table and grows monotonically inside one VM
invocation. No GC — the entire arena drops with the VM at `invoke`
exit.

The BcOp variant carries the type discrimination (`ListGet` knows it
indexes the list arena; `DictLookup` knows it consults the dict
arena) so the slot itself stays untyped — no per-op tag dispatch.

Pros:

- Dispatch loop stays `u64`-shaped: cmp / arith / jump ops unaffected.
- Arena per-type keeps cache locality reasonable (no enum-tag splat).
- Lifetime model is trivial — arenas die with `BcRunOutcome`; nothing
  escapes the VM.

Cons:

- Handles are VM-local; can't outlive `invoke`. The host-fn encode
  lane has to materialise into a real `Value` before returning (acceptable
  — phase 4a already pays that cost on every host call).
- Trace-JIT integration (phase 4c) needs a bridge — the trace recorder
  expects pointer-shaped values, not handles. Mitigated by deferring
  trace-JIT memory layout work to phase 4c (see §5).

Verdict: ship.

### Option C — shared `StringRef`/`ListRef` pointers

Slots carry `*const StringRef`-shaped pointers identical to the
trace-JIT arena layout. Pros: phase-4c trace-JIT can reuse the same
memory. Cons:

- `unsafe` everywhere (we already `#![forbid(unsafe_code)]` in
  `relon-bytecode`); we'd have to lift the forbid.
- Lifetime management is a non-trivial design problem — the bytecode
  VM is single-threaded and short-lived, the trace-JIT arena is long-lived.
  Sharing the layout doesn't automatically share the lifetime.
- Doesn't actually buy us trace-JIT speed today — trace-JIT recorder
  isn't wired to bytecode (phase 4c work).

Verdict: defer; revisit if the phase-4c trace-JIT bridge measures a
real perf win from layout-sharing.

## 3. Arena design

```rust
// crates/relon-bytecode/src/arena.rs
pub struct ListArena { entries: Vec<Arc<Vec<u64>>> }
pub struct DictArena { entries: Vec<Arc<Vec<(Arc<str>, u64)>>> }
pub struct StringArena { entries: Vec<Arc<str>> }
```

- **List element representation**: `Vec<u64>` mirrors the bytecode VM's
  operand-stack slot shape. A heterogeneous list-of-Value escape
  surfaces as `BcVmError::HostFnReturnTypeMismatch` at the boundary
  (phase 4a envelope reused).
- **Dict key**: `Arc<str>` — bytecode VM only models string-keyed
  dicts (matches the IR `DictGetByStringKey` op surface). Numeric-key
  dicts route through the cranelift / tree-walker backends.
- **Allocation**: monotonic; `alloc(value) -> handle` pushes a new
  entry. Handle stability: never rewritten; never reordered. Re-use
  is out of scope (deferred until we measure allocator pressure).
- **Cloning**: `arena.get(handle).clone()` produces a new `Arc` —
  refcount bump only. The arena keeps a strong reference per slot;
  consumers hold transient `Arc` clones for the duration of an op.

Total surface area for phase 4b: three arenas + accessor helpers,
~120 LoC.

## 4. BcOp table expansion (planned)

Group A — strings (phase 4b initial):

| Op | Shape | Notes |
|----|-------|-------|
| `StrConst(idx)` | `[] -> [str_handle]` | Pre-built handle into a per-function string pool. |
| `StrLen` | `[str] -> [i64]` | Code-point count to match tree-walker. |
| `StrConcat` | `[str, str] -> [str]` | Allocates a fresh arena slot. |
| `StrEq` | `[str, str] -> [bool]` | Refcount fast-path; falls back to byte cmp on miss. |

Group B — lists (phase 4b initial):

| Op | Shape | Notes |
|----|-------|-------|
| `MakeList(len)` | `[v0..v_{n-1}] -> [list]` | Pops `len` slots, allocates. |
| `ListGetInt` | `[list, i64] -> [i64]` | Bounds-checked; trips `IndexOutOfBounds`. |
| `ListPush` | `[list, v] -> [list]` | Copy-on-write semantics (clone arena slot if shared). |
| (`ListLen` from phase 3) | repurposed | Phase 3 emitted a witness no-op; phase 4b makes it consult the arena. |

Group C — dicts (phase 4b follow-up, scope-limited):

| Op | Shape | Notes |
|----|-------|-------|
| `MakeDict(len)` | `[k0,v0..] -> [dict]` | Pops `2*len` slots. |
| `DictLookupStr` | `[dict, str] -> [v]` | Returns `Null` on miss (matches tree-walker). |

Closure ops (`MakeClosure / CallClosure`) stay deferred (phase 4d / M3).

## 5. Phase scope — what lands NOW vs follow-up

**This phase (4b scaffold)** — bound by the wide-LoC discipline the
predecessors share:

1. `arena` module (List / Dict / String) with public alloc / get /
   clone surface.
2. `BcOp::MakeList(len)` + `BcOp::ListGetInt` + the dispatch arms.
3. `apply_stack_effect` extensions for the new ops.
4. Round-trip tests: `MakeList` + `ListGetInt` consistency, bounds
   trap on out-of-range index, arena handle survives multiple ops.

**Phase 4b-continuation (follow-up commit / agent)**:

- `ListPush` + copy-on-write semantics.
- `StrConst` + per-function string pool wire-up + `StrLen` / `StrConcat` /
  `StrEq`.
- `MakeDict` + `DictLookupStr`.
- `compile.rs` lift: drop the `unsupported("ListGetByIntIdx")` /
  `unsupported("DictGetByStringKey")` bounces in favour of real emit.

**Phase 4c (planned)** — trace-JIT hot counter:

- Per-`BcFunction` call counter; threshold-trip routes through the
  recorder bridge.
- Bridge layer translates arena handles into trace-JIT arena
  pointers (the layout-sharing question from Option C resurfaces here).
- `cmp_lua_dict_list_trace` integration.

## 6. Non-goals (explicit defer)

- F64 list element lane — phase 4b uses `u64` slot which carries f64
  via `to_bits`. A dedicated `ListGetFloat` op stays a "if we ever
  measure that the bit-cast hurts" item.
- Heterogeneous list / dict elements — phase 4b only handles
  type-uniform lists (matching the IR `ConstList*` surface).
- Arena defrag / slot reuse — wait until we have a benchmark showing
  pressure.
- Send + Sync on arenas — bytecode VM is single-threaded; we'll add a
  `!Send` marker when the trace-JIT bridge lands if needed.

## 7. Risks

- **Arena pressure**: tight loops that build / drop lists every
  iteration leak monotonically. **Mitigation**: per-invoke arena
  caps + the existing `max_steps` throttle. Reuse work after we have
  numbers.
- **Phase-4c divergence**: if the trace-JIT bridge demands a pointer
  layout, we'll re-litigate Option C. The current handle-based model is
  not load-bearing for trace-JIT — it's an internal-only contract.
- **Host-fn list/dict encode**: phase 4a's `encode_value_for_ret` only
  speaks scalars. We need a separate `encode_list_for_ret` /
  `decode_list_arg` pair when host fns start returning lists. Out of
  scope for the phase-4b scaffold; surfaces as
  `HostFnReturnTypeMismatch` until phase 4b-continuation.

## 8. Commit shape (phase 4b scaffold)

1. `docs(internal): RFC phase 4b memory model` — this file.
2. `feat(bytecode): add arena module (List/Dict/String)` — pure
   addition, no callers yet.
3. `feat(bytecode): MakeList + ListGetInt ops + dispatch + tests` —
   first round of list ops + the `apply_stack_effect` table extension
   + bytecode-VM tests pinning the round-trip + bounds-trap.
4. (deferred) compile.rs visitor lift + Str ops + Dict ops + corpus
   four-way activation.
