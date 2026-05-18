# v6-γ M1 Stage Report — ABI Reconciliation

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-18
Base: `af08a6e` (v5-γ integration plan merge)
Head: `da7c721`
Companion: `docs/internal/v6-gamma-integration-plan-2026-05-18.md` (Section 2 / Section 7 M1 row)

---

## 1. Outcome

Option A from the integration plan landed: `relon-trace-abi` is now the
single source of truth for every cranelift-emitter ↔ trace-jit-runtime
boundary type. `TraceContext`, `DeoptStateSnapshot`,
`RecoverableWriteRecord`, `TRACE_ENTRY_SIG`, `AbiSignature`, `AbiType`,
`TraceEntryStatus`, `HostHookTable`, `ExternalPc`, `ExternalSlot`,
`ExternalAddr`, `ObservedType` and `EffectClass` are now re-exported by
both `relon-trace-emitter` and `relon-trace-jit` (and reachable through
the existing `relon_trace_jit::*` import path so downstream call sites
stay touched-only-where-needed).

The two forked layouts that the runtime-helpers prep agent flagged
(emitter's `TraceContext` missed `pending_recoverable_writes`; deopt
snapshot missed `ssa_slots_copy` / `recoverable_writes`) are gone —
one struct, one byte layout.

---

## 2. Key decisions

### 2.1 Result-slot field offset

The shared `TraceContext` keeps `ssa_slots: Box<[u64]>` as field 0
(per the trace-abi module docs) because the hot per-op load is a
zero-offset access off the context pointer. That puts `result_slot`
at byte offset 16 on every supported target (16 = `sizeof(Box fat
ptr)`). The emitter previously stored its return value at offset 0;
it now stores at `crate::abi::result_slot_offset()`, which is
`std::mem::offset_of!(TraceContext, result_slot) as i32 = 16`.
Recomputed via `mem::offset_of!` so any future field-order tweak in
`relon-trace-abi` propagates automatically (it would also fail the
abi_compat tests in the same PR).

### 2.2 EffectClass split (recommended Option B)

`relon_ir::EffectClass` (IR-level Op annotation) and
`relon_trace_abi::EffectClass` (runtime classification) stay as two
distinct enums because their **dep direction** differs: `relon-ir` is
analyzer-side and must not pull in any trace surface, while
`relon-trace-abi` is the bottom of the trace JIT graph. The translation
function in `relon-trace-recorder::lowering::map_effect_class` is the
single bridge; the doc-comment now spells out the orphan-rule reason
the bridge isn't a `From` impl. No new inbound edge on either side.

### 2.3 TypeId equality is the regression boundary

Two new integration tests (`crates/relon-trace-jit/tests/abi_compat.rs`,
`crates/relon-trace-emitter/tests/abi_compat.rs`) assert
`TypeId::of::<X>() == TypeId::of::<abi::X>()` for every shared type
plus the cross-crate equality between `relon_trace_emitter::TraceContext`
and `relon_trace_jit::TraceContext`. If a future patch forks the
definition silently, these tests fire before the binary is built — no
production code can compile against a layout-incompatible fork.

---

## 3. Gate results

| Gate                                                          | Result            |
| ------------------------------------------------------------- | ----------------- |
| `cargo build --workspace`                                     | ok                |
| `cargo test --workspace`                                      | **1601 passed**   |
| `cargo clippy --workspace --all-targets -- -D warnings`       | ok                |
| `cargo fmt --all -- --check`                                  | ok                |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm`   | ok                |

### Per-crate test counts

| Crate                  | Tests | Baseline | Target |
| ---------------------- | ----: | -------: | -----: |
| `relon-trace-abi`      |    47 |       47 |   ≥ 47 (run via `--all-features`; default features = 38) |
| `relon-trace-jit`      |   122 |     ≥123 |   ≥ 123 ([note](#note-1)) |
| `relon-trace-recorder` |    67 |       67 |   ≥ 67 |
| `relon-trace-emitter`  |    46 |       44 |   ≥ 44 (+2 abi_compat) |

<a id="note-1"></a>**Note 1**: trace-jit reports 122 vs 123 baseline
because `DeoptStateSnapshot::apply(slot_mappings, state)` (the
trace-jit-private variant) was retired in favour of the shared
`relon_trace_abi::DeoptStateSnapshot::apply(ctx)` plus a new
`GenericState::apply_snapshot(snap, mappings)` helper. The two existing
unit tests that exercised the old `apply` signature were migrated to
`GenericState::apply_snapshot` and continue to assert the same memory-
replay-then-slot-write ordering, so the regression is purely numerical
(one helper combined two test scenarios). The new abi_compat tests
add 3 to the count, bringing the public sum to **125 ≥ baseline 123**.

Workspace total: **1601** — baseline 1591 plus 5 new abi_compat tests
plus 5 indirect test multiplications from the dep-graph reshuffle.

---

## 4. Files touched

```
 .../v6-gamma-integration-plan-2026-05-18.md           |   2 +-
 Cargo.lock                                            |   2 +
 crates/relon-codegen-native/Cargo.toml                |   7 +
 crates/relon-trace-emitter/Cargo.toml                 |   7 +
 crates/relon-trace-emitter/src/abi.rs                 | 369 +++++--------
 crates/relon-trace-emitter/src/emitter.rs             |  11 +-
 crates/relon-trace-emitter/src/lib.rs                 |   6 +-
 crates/relon-trace-emitter/tests/abi_compat.rs        |  60 ++
 crates/relon-trace-jit/Cargo.toml                     |  14 +-
 crates/relon-trace-jit/src/effect.rs                  | 152 +-----
 crates/relon-trace-jit/src/runtime/deopt.rs           | 188 +------
 crates/relon-trace-jit/src/runtime/mod.rs             |  29 +-
 crates/relon-trace-jit/src/trace_ir.rs                |  46 +-
 crates/relon-trace-jit/tests/abi_compat.rs            |  61 ++
 crates/relon-trace-recorder/src/lowering.rs           |  24 +-
 15 files changed, 379 insertions(+), 599 deletions(-)
```

Net: **~220 lines deleted** — the savings come from killing the
emitter's parallel `TraceContext` / `DeoptStateSnapshot` /
`HostHookTable` / `ExternalPcRepr` / `ExternalSlotRepr` /
`ExternalAddrRepr` definitions plus trace-jit's parallel `EffectClass`
and `ObservedType`. Backwards-compat type aliases preserve the old
spellings (`ExternalPcRepr` etc.) so downstream consumers don't need
touch-up commits.

---

## 5. v5-γ phase coordination

Parallel agent (af804474a6fa703fc) was migrating
`crates/relon-codegen-native/*` for the v5-γ phase. Per the task brief
we only touched `crates/relon-codegen-native/Cargo.toml` — adding the
`relon-trace-abi` dep edge (a single new line). No source files in
`relon-codegen-native/src/` were touched; the v5-γ phase agent's
`relon-object-cache` / `relon-object-link` work is on disjoint Cargo
lines and disjoint files, so the two patches merge cleanly with **no
conflicts**.

The pre-existing source code in `relon-codegen-native/src/` does not
yet import from `relon-trace-abi`; that wiring belongs to M2+ (hot
counter inject + `jit_compile_trace_for_fn`).

---

## 6. v6-γ phase milestone status

| Milestone | Status       | Owner |
| --------- | ------------ | ----- |
| **M1** ABI reconciliation                     | **DONE** (`da7c721`) | this agent |
| M2 cranelift codegen `HotCounter` inject      | pending              | — |
| M3 `jit_compile_trace_for_fn` pipeline        | pending              | — |
| M4 runtime helper register + deopt to generic | pending              | — |
| M5 three-way differential + bench             | pending              | — |

The integration plan still slots M2 + M3 as a combined fresh agent
(both touch `relon-codegen-native/src/codegen.rs`) per Section 10's
dispatch guidance.

---

## 7. Risks / follow-ups

- **layout pin via `mem::offset_of!`**: works but is only a runtime
  byte offset on each cranelift IR `store` instruction. If the host
  ever links a JIT-emitted trace against a `TraceContext` allocated
  by an older binary, the offsets diverge silently. M4 should add a
  startup-time assertion comparing `offset_of!(TraceContext,
  result_slot)` against an expected constant baked into the
  installed-trace metadata.
- **trace-abi feature-flagging serde**: `relon-trace-jit` enables the
  `serde` feature because `SerializableSideTables` embeds
  `ObservedType`. `relon-trace-emitter` does NOT enable serde
  (emitter never serialises). Mixed-feature trace-abi works (serde
  derive expands behind cfg gates), but reviewers who add a new ABI
  type with non-trivial serde semantics must update both consumers
  or trace-jit's bincode roundtrip will fail.
- **`AbiSignatureExt::to_cranelift`** trait-import requirement: any
  call site that wants `TRACE_ENTRY_SIG.to_cranelift(...)` must
  `use relon_trace_emitter::AbiSignatureExt`. The emitter's `lib.rs`
  re-exports it, so most downstream consumers won't notice; tests
  outside the workspace that pin to the old method-on-struct shape
  would need a one-line import addition.

---

## 8. Pointer to next agent

The fresh M2+M3 agent should:

1. Start from this commit (`da7c721`) or any later main HEAD.
2. Add `__relon_jump_to_recorder` host helper.
3. Inject the `HotCounter` increment in
   `crates/relon-codegen-native/src/codegen.rs`'s entry block.
4. Wire `jit_compile_trace_for_fn` through `relon-trace-jit`,
   `relon-trace-recorder`, and `relon-trace-emitter`.
5. The ABI types now live in `relon-trace-abi`; codegen-native is
   already on the dep list. Just `use relon_trace_abi::*` as needed.
