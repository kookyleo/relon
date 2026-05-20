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
| `F-D9-cmp_lua-958LoC.patch` | F-D9 cmp_lua trace-JIT wiring; agent `ae896e6238a60836b` API-529 termination | F-D7-B / F-D8-B (recorder learns to auto-lower `s + t` / `s.contains(_)` / `d[k]` / `xs[i]` AST shapes) land in `main`, replacing the hand-built JIT entry pattern this patch encodes. After that, this patch encodes an obsolete shape and should be deleted. |

When removing an entry, also delete the corresponding row from this
table.
