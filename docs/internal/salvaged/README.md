# `docs/internal/salvaged/`

Patch / source archives rescued from agents that terminated mid-work
(API limits, OOM, etc) but whose output was worth keeping for the next
attempt.

Each artifact here is **transient** — once the work it captures lands
on `main` in committed form, the file should be removed. Keeping
salvaged patches around past their useful life clutters `internal/`
and risks future devs applying a stale patch to a moved-on tree.

## Current contents

| File | Source | Removable when |
|---|---|---|
| `F-D9-cmp_lua-958LoC.patch` | F-D9 cmp_lua trace-JIT wiring; agent `ae896e6238a60836b` API-529 termination | F-D7-D + F-D8-D (IR-emit side — analyzer + relon-ir + stdlib bodies emit `Op::Add(IrType::String)` / `Op::Call{contains}` / `Op::DictGetByStringKey` / `Op::ListGetByIntIdx` instead of routing through the tree-walker's `try_index_method`) land in `main`, **and** the cmp_lua W3/W4/W5/W6 hand-built JIT entries are deleted in favour of `TraceRecordingEvaluator`-driven traces. F-D7-B / F-D8-B (commits `516c7a2` / `d2c478f`) landed the receiver side only — the patch's hand-built encoding is still the live shape the bench file uses. |

When removing an entry, also delete the corresponding row from this
table.
