# `docs/internal/salvaged/`

Patch / source archives rescued from agents that terminated mid-work
(API limits, OOM, etc) but whose output was worth keeping for the next
attempt.

Each artifact here is **transient** — once the work it captures lands
on `main` in committed form, the file should be removed. Keeping
salvaged patches around past their useful life clutters `internal/`
and risks future devs applying a stale patch to a moved-on tree.

## Current contents

_(empty — no live salvaged artifacts)_

When removing an entry, also delete the corresponding row from this
table.

## History

| File | Removed | Reason |
|---|---|---|
| `F-D9-cmp_lua-958LoC.patch` | 2026-05-20 | F-D7-D (`4a4341f`) + F-D8-D (`7219e70`) landed the IR-emit side AND deleted the cmp_lua W3/W4/W5/W6 hand-built JIT entries in favour of `TraceRecordingEvaluator`-driven traces. The patch's hand-built encoding had no live consumer left in the tree, so per this directory's transience rule it was removed. The shape it encoded is reconstructible from those two merges if a future micro-bench wants the byte-identical floor back. |
