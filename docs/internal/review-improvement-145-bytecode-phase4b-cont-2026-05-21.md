# review #145 — bytecode VM phase 4b-continuation stage report

Date: 2026-05-21. Branch: `worktree-agent-a920e8960f5f87b06`.
Base: `cb9f889` (phase 4b scaffold merge).

## Sub-task status

| sub-task | scope | status | LoC delta |
| -------- | ----- | ------ | --------- |
| A | compile.rs IR-lift for `Op::ListGetByIntIdx` / `Op::DictGetByStringKey` | done (visitor wires both directly into `BcOp::ListGetInt` / `BcOp::DictLookupStr`) | +57 src |
| B | `BcOp::StrConst`/`StrLen`/`StrConcat`/`StrEq` + per-function `string_pool` | done (all four ops dispatch through `VmMemory.strings`) | +91 op.rs / +75 vm.rs / +153 tests |
| C | `BcOp::MakeDict` + `BcOp::DictLookupStr` | done (last-write-wins on dup keys preserved) | shared above |
| D | `ListPush` copy-on-write | done (`Arc::get_mut` hot path; clone-on-shared fallback) | +33 arena.rs / +57 tests |
| E | host-fn return lane for `String` / `ListInt` | done (`encode_value_for_ret` lifts both into per-call arenas) | +37 vm.rs / +152 tests |

All five sub-tasks landed. Total: +953 LoC across 6 files (953 insertions / 10 deletions vs base).

## Test verify

Workspace test count: **2130 passed / 0 failed** (baseline before this stage: 2114; net +16).

New tests (all hand-built BcFunction sandboxes):

- bytecode_sandbox: 12 new — list_push (2), string ops (4), dict ops (3), host-fn lanes (3).
- compile.rs unit tests: 2 new — IR-lift pin for `ListGetByIntIdx` and `DictGetByStringKey`.
- arena.rs unit tests: 2 new — `push_cow` single-owner + shared-owner paths.

Gate check (all green):
- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — 2130 passed / 0 failed
- `cargo check --target wasm32-unknown-unknown -p relon-wasm` — clean

## Bytecode workload coverage

The VM mechanics for list / dict / string round-trips through arenas now
work end-to-end. Sandbox tests cover:

- `[10, 20, 30]` build + indexed read + append (refcount 1 hot path + refcount > 1 clone path).
- `"héllo".chars().count()` via `StrConst`+`StrLen`; `"foo" + "bar"` concat; byte-equal compare.
- `{ a: 1, b: 2 }[a]` build + lookup; duplicate-key last-write-wins; miss → `IndexOutOfBounds`.
- Host fn returning `Value::String("héllo")` round-trips through `IrType::String` lane → `StrLen` reads 5.
- Host fn returning `Value::List<Int>` round-trips through `IrType::ListInt` lane → `ListGetInt` resolves elements.

**cmp_lua W3 / W5 / W6 4-way**: not yet reachable. The from-source pipeline still length-folds
`Op::ConstListInt` / `Op::ConstString` (cheap `.length()` path), so list literals don't
mint real handles end-to-end. Real 4-way agreement for those workloads also requires
closures + iterators + `range`/`map`/`reduce` stdlib — all M3 work. The phase 4b-continuation
mechanics make those workloads **mechanically reachable** when the upstream lowering pipeline
catches up; today the gain is "the VM can run hand-built IR that exercises the full handle
discipline without dropping into `UnsupportedOp`".

## Risk + trade-offs

- **`ConstListInt` length-fold preserved**: changing the lift to emit `MakeList` would also
  require switching `ListLen` (today a witness no-op) to consume a handle + push length.
  Punted to phase 4c to keep this stage's scope tight; the existing `length()` corpus tests
  stay green.
- **Heterogeneous host-fn list returns**: `Value::List<String>` / `Value::List<Float>` still
  surface as `HostFnReturnTypeMismatch`. The `ListInt` lane handles the bulk of corpus
  shapes; widening is mechanical when needed.
- **Dict storage is `Vec<(Arc<str>, u64)>`**: linear scan. Good enough for the ≤16-entry
  static fingerprints the recorder already specialises on; phase 4c can swap in a
  fingerprint-aware lookup when the trace-JIT bridge lands.

## Phase 4c blueprint

1. Switch from-source `Op::ConstListInt` / `Op::ConstString` from length-fold to real
   handle emission (`MakeList` / `StrConst`). Update `Op::Call(length)` inlining so the
   wrapped `Op::ReadStringLen` lifts to `BcOp::StrLen` / new `BcOp::ListLen`-as-handle.
2. Wire `Op::Call(stdlib::concat)` / `Op::Call(stdlib::substring)` inlining to the new
   `BcOp::StrConcat` + a `BcOp::Substring` slot.
3. Audit trace-JIT recorder's `RecorderState::emit_dict_lookup` / `emit_list_get` for
   handle bridge — phase 4c is the natural integration point.
4. Bench: re-run cmp_lua W5/W6 4-way once closures / `range` land in the VM (M3 dep).

## Branch + worktree

- Branch: `worktree-agent-a920e8960f5f87b06`
- Worktree: `/ext/relon/.claude/worktrees/agent-a920e8960f5f87b06`
- Final SHA before this report: `f0678c7`
- Commits in stage (5):
  - `2e22da3` feat(bytecode): add list/dict/string op surface for phase 4b cont
  - `00f4113` test(bytecode): sandbox pins for list/dict/string op dispatch
  - `f0b547c` test(bytecode): pin host-fn String/ListInt return lanes
  - `e79bc90` style(bytecode): cargo fmt for phase 4b-continuation surface
  - `f0678c7` test(bytecode): IR-lift + push_cow unit tests for phase 4b cont
