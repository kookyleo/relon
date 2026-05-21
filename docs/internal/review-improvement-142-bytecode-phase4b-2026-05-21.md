# M2-B phase 4b scaffold — bytecode VM memory model (stage report)

Scope: pick + ship the bytecode VM's list / dict / string memory
model and land the first list ops on top of it. Strings / dicts /
`ListPush` are explicit phase-4b-continuation deliverables; trace-JIT
hot counter is phase 4c.

## Option chosen — B (handle + per-VM arena)

- **A — `Value` enum on the stack**: rejected. Every push / pop
  clones a 32-byte tagged enum; scalar lane regresses 3-5x against
  the `u64` slot floor we just established.
- **B — handle + per-type arena**: shipped. Slot stays `u64`; arenas
  are owned by the VM invocation and drop with `BcRunOutcome`.
- **C — pointer layout shared with trace-JIT**: deferred. Would force
  unsafe + complex lifetime modelling for no immediate perf win (the
  trace-JIT bridge is phase 4c). Re-litigated when the bridge needs it.

## Arena design

```
ListArena   slots: Vec<Arc<Vec<u64>>>         u32 handles, monotonic
DictArena   slots: Vec<Arc<Vec<(Arc<str>,u64)>>>
StringArena slots: Vec<Arc<str>>
```

Bundled in `VmMemory`; the dispatch loop borrows the bag mutably as
a unit so multi-arena ops (none yet) don't fight partial borrows.
No GC / slot reuse — arenas drop with the call; ~120 LoC end-to-end.

The BcOp variant carries the type discrimination
(`BcOp::ListGetInt` knows it consults the list arena), so the slot
stays untyped and the existing `match` dispatch arms unchanged.

## Ops landed

| Op | Shape | Notes |
|----|-------|-------|
| `BcOp::MakeList { len }` | `[v0..v_{n-1}] -> [list_handle]` | Pops `len` operands, allocates arena slot. |
| `BcOp::ListGetInt` | `[list, idx] -> [elem]` | Bounds-checked; trips `IndexOutOfBounds`. |

`apply_stack_effect` tags both as `Snapshot` producers (the handle /
element value can't be derived from locals+consts alone at
partial-resume time).

## 改造点 + LoC

- `crates/relon-bytecode/src/arena.rs` (+387, new): three arenas +
  composite `VmMemory` + `ArenaError` + 6 unit tests.
- `crates/relon-bytecode/src/op.rs` (+30): two new `BcOp` variants
  with doc shape comments.
- `crates/relon-bytecode/src/vm.rs` (+59): `VmMemory` per-invoke
  state, dispatch arms for `MakeList` / `ListGetInt`, `arena_to_vm_error`
  lift helper.
- `crates/relon-bytecode/src/compile.rs` (+25): `apply_stack_effect`
  arms for the two new variants.
- `crates/relon-bytecode/src/lib.rs` (+2): module + re-exports.
- `crates/relon-bytecode/tests/bytecode_sandbox.rs` (+233): 7 new
  integration tests pinning round-trip, first / last / empty, oob +
  negative index, multi-handle isolation, stack underflow,
  declaration-order, per-invoke arena reset.
- `docs/internal/rfc-m2-b-phase4b-memory-model-2026-05-21.md` (new):
  Option A/B/C comparison + ops table + scope split.
- `docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md`
  (+5 / -2): phase 4b-scaffold marked landed; 4b-continuation + 4c
  split out as explicit follow-ups.
- `crates/relon-codegen-native/src/glob_helper.rs` (fmt drift): folded
  in from main (phase 4a stage report's pattern).

## Test verify

- `cargo test -p relon-bytecode`: 67 passed (3 lib + 6 arena unit +
  38 sandbox + 14 smoke + 6 partial_resume), 0 failed.
- `cargo test --workspace`: 2114 passed (≥ 2101 phase-4a baseline +
  this branch's 13 new tests). 0 failed.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --all --check`: clean (after fmt fold-in).
- `cargo check -p relon-bytecode --target wasm32-unknown-unknown`:
  clean.

The integration tests pin four contract surfaces:

1. **Happy path** — round-trip + first / last / order-preservation +
   multi-handle isolation.
2. **Bounds prong** — `IndexOutOfBounds` on upper / negative /
   `i64::MIN` / empty-list indices; lifts through
   `RuntimeError::WasmIndexOutOfBounds`.
3. **Compiler-bug envelope** — `MakeList` with too few operands
   trips `StackUnderflow`.
4. **Per-invoke reset** — back-to-back `vm.invoke` calls each start
   with a fresh arena.

## Out of scope (phase 4b-continuation / 4c)

- `BcOp::ListPush` + copy-on-write semantics.
- String ops (`StrConst` + per-function string pool, `StrLen` /
  `StrConcat` / `StrEq`).
- Dict ops (`MakeDict` + `DictLookupStr`).
- Compile-pass lift: drop `unsupported("ListGetByIntIdx")` /
  `unsupported("DictGetByStringKey")` bounces in favour of real
  emit + four-way differential row activation.
- Closure ops (`MakeClosure` / `CallClosure`) — still M3.
- Host-fn boundary `encode_list_for_ret` / `decode_list_arg` pair —
  surfaces as `HostFnReturnTypeMismatch` until 4b-continuation.

## Phase 4c blueprint (trace-JIT hot counter)

- Per-`BcFunction` call counter on the VM (one `AtomicU64`); ticks
  on `invoke` entry, threshold-trips through the recorder bridge.
- Bridge layer: translate arena handles into trace-JIT arena
  pointer-shapes. This re-opens the phase-4b RFC's Option C
  question — at trace-JIT entry we need the recorder's `RecordedValue`
  to carry a layout the JIT can re-materialise. Two options:
  - copy the arena slot into the trace-JIT memory (cost on hot
    transitions; cheap on cold);
  - share the `Arc<Vec<u64>>` with the trace-JIT runtime arena
    (zero copy; needs a `Send + Sync` lift on the arenas).
  Decision deferred to the trace-JIT bridge agent.
- `cmp_lua_dict_list_trace` bench row activation once the bridge
  measures non-zero `four_way_all_agree` rate on a list-heavy
  workload.
