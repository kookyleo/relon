# `scripts/` — maintainer utilities

| Script | Purpose |
| --- | --- |
| `verify.sh` | Local green-gate: fmt + build + clippy + test + `relon-fmt --check`, mirroring the CI `stable` job. Run before shipping changes |
| `install-hooks.sh` | Symlink the version-controlled git hooks (below) into `.git/hooks/`. Run once after a fresh clone |
| `pre-commit.sh` | Advisory pre-commit hook: lists staged files so cross-task scope creep is visible at commit time. Never blocks |
| `perf-flamegraph.sh` | CPU flamegraphs for the `relon-bench` `profile_alloc` workloads (SVGs under `target/flamegraph/`) |
| `bench_quiescence.sh` | Put the bench host into a quiescent state (governor, SMT, turbo) before a measurement round; `--check` for read-only verification |
| `install_luajit_2_1.sh` | Optional: user-prefix LuaJIT 2.1 install for system-vs-vendored bench comparisons (the bench crate vendors LuaJIT by default) |
