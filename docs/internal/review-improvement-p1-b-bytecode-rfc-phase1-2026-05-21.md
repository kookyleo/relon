# P1-B Stage Report — M2-B Bytecode JIT Integration (RFC + Phase 1)

Date: 2026-05-21
Worktree: `.claude/worktrees/agent-ac8a6a5af256fa8c2`
Branch: `worktree-agent-ac8a6a5af256fa8c2` (base `ea29a7d`)

## RFC key decisions

`docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md`:

1. **Phase split 1 → 4.** Gate hook (S) → bench row + first guarded
   `BcOp` consult (S) → hot counter + recorder bridge (M) → Op
   coverage subset (L). All phases ≤ M.
2. **Bench first, perf last.** Phase 2 wires `four_way` into a bench
   so phase 3 / 4 have a measured baseline. Risk: bytecode VM perf
   vs tree-walker untested.
3. **Capability timing diff stays intentional** (cranelift = build,
   bytecode + tree-walker = dispatch).
4. **Closure / `MakeClosure` deferred to M3.**
5. **`BcOp` audit.** ~40 IR Op variants listed as phase 4 targets.

## Phase 1 change set

| File | LoC | Purpose |
|------|-----|---------|
| `vm.rs` | +48 / −2 | `CapabilityVtable.gate: Option<Arc<dyn CapabilityGate>>` + manual `Debug` (trait object not `Debug`) + `set_gate` / `gate` accessors |
| `evaluator.rs` | +20 / −1 | `BytecodeEvaluator::with_capability_gate(gate)` threads gate into default VM config |
| `lib.rs` | +8 | Module doc → RFC + phase 1 → 2 hand-off |
| `tests/bytecode_sandbox.rs` | +60 | 2 new tests: counting-gate evaluator round-trip + direct vtable assertion |

Total: +136 / −3 across 4 files, 2 new tests.

## Verification

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: **2029 / 0** (baseline 2027 + 2 new).
- `cargo check --target wasm32-unknown-unknown` (eval-api / evaluator
  / bytecode): clean.
- Phase 1 does **not** wire the gate into the dispatch loop — the
  counting-gate test asserts `gate.hits == 0` after a scalar
  `run_main`, confirming the hook is parked, not consulted. Matches
  the RFC's phase split.

## Audit surface

- `CapabilityVtable.gate` is the only path bytecode exposes to the
  unified P0-B policy. Phase 2 consults from dispatch + new
  `BcOp::CheckCap`; until then, dormant.
- Trait object isn't `Debug`; vtable gets a hand-rolled `Debug`
  printing `has_gate: bool` only.

## Next phase plan

- **Phase 2**: `BcOp::CheckCap` + dispatch-time gate consult (fall
  back to grant table when no gate set) + wire `Backend::Bytecode`
  into `cmp_lua_dict_list_trace` four-way subset. (S)
- **Phase 3**: hot counter + trace-recorder bridge. (M)
- **Phase 4**: `BcOp` coverage subset — Let / LoadField / StoreField
  / Select + record builder. (L)

## Output

- Branch: `worktree-agent-ac8a6a5af256fa8c2`
- Commits:
  - `9ccc3e2` `feat(bytecode): wire CapabilityGate trait for future native dispatch`
  - `5a6ecaa` `docs(internal): RFC for M2-B bytecode JIT integration`
- Worktree: `/ext/relon/.claude/worktrees/agent-ac8a6a5af256fa8c2`
- Base: `ea29a7d750be540fd6aca9763a3a5d5000d61bf6`
- Not pushed.
