# v6-λ-机器 quiescence + λ-1 LuaJIT install — Stage report

**Status**：DONE-MET-TARGET（2026-05-19）
**Base**：`52f9a38 docs(internal): v6-λ-0 stage report + plan §21 + bench appendix`
**前置**：λ-0 bench methodology hardening 已落 main（6 陷阱硬化 + 12 个 validators）

---

## 0. 目标回顾

本阶段为 LuaJIT 对照 phase（λ-2 起）做机器与依赖准备：

1. **λ-机器 quiescence**：把 rigorous plan §6 的机器 quiescence 要求落成
   - 可执行脚本 (`scripts/bench_quiescence.sh`)
   - bench harness 启动自检 (`crates/relon-bench/src/quiescence.rs`)
   - integration test (`crates/relon-bench/tests/quiescence_check.rs`)
2. **λ-1 LuaJIT install**：把 LuaJIT 2.1-stable 接入 bench
   - 用户可执行脚本 (`scripts/install_luajit_2_1.sh`)
   - mlua dev-dep（vendored LuaJIT，路径阻力最小）
   - `lua_boundary_calibrate` bench 行 — Lua boundary 基线
   - smoke test (`crates/relon-bench/tests/lua_smoke.rs`)

碎片化写入（filled in incrementally as each sub-step lands）。

---

## 1. λ-机器 quiescence

### 1.1 `scripts/bench_quiescence.sh`

用户级（非 agent 执行）脚本，bench 前手动跑。需要 sudo（governor / no_turbo
是 root-only sysfs 节点）。脚本本身 fail-fast 退出，不会 silently 误导用户。

主要步骤：

1. `cpupower frequency-set -g performance` — 把所有 CPU 切到 performance governor。
2. `echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo` — 关 turbo
   boost，避免 sample 间 freq 漂移。
3. 报告 thermal zones 当前温度（`/sys/class/thermal/thermal_zone*/temp`）。
4. 跑 `perf stat -a sleep 5` 测 5 秒 baseline noise。如果 context-switches / sec
   / core > 100，exit non-zero，提示 "machine too noisy"。
5. 提示用户 bench 命令应通过 `taskset -c 4-7` 绑核。

详 [scripts/bench_quiescence.sh](../../scripts/bench_quiescence.sh)。

### 1.2 `crates/relon-bench/src/quiescence.rs`

Rust 侧 quiescence 自检模块：

- `verify_quiescence() -> Result<QuiescenceReport, QuiescenceError>` —
  读 `/sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`、
  `/sys/devices/system/cpu/intel_pstate/no_turbo`、
  `/proc/loadavg`、`/sys/class/thermal/thermal_zone*/temp`，统一检查。
- 公共 `QuiescenceReport`：每个 governor、no_turbo 状态、load avg、thermal 温度。
- `QuiescenceError`：governor 不是 performance、no_turbo ≠ 1、load > 1.0 等都
  分别枚举。

`trace_jit_hot_loop` bench 在 `bench_hot_loop` 入口调用，**未达 quiescent 即
`panic!("machine not quiescent")`**，可用 `RELON_BENCH_FORCE_RUN=1` 覆盖（dev
machines 不一定能锁 governor）。

### 1.3 `crates/relon-bench/tests/quiescence_check.rs`

- 单测试 `verify_machine_quiescence`，标 `#[ignore]`（normal `cargo test` 不
  gate 在它上）。
- 跑命令：`cargo test -p relon-bench --test quiescence_check -- --ignored`。
- 输出 QuiescenceReport 全部字段供人眼检视。

---

## 2. λ-1 LuaJIT install

### 2.1 `scripts/install_luajit_2_1.sh`

用户级（非 agent 执行）非交互式脚本，安装 LuaJIT 2.1-stable 到 user-writable
路径（无 sudo），便于本机 + 远程 bench 机一致复现：

1. `git clone https://github.com/LuaJIT/LuaJIT.git /tmp/LuaJIT-src`
2. `git checkout v2.1` （LuaJIT 长期 stable 分支）
3. `make CCDEBUG=-g PREFIX=/tmp/luajit-2.1`
4. `make install PREFIX=/tmp/luajit-2.1`
5. 设 `PKG_CONFIG_PATH=/tmp/luajit-2.1/lib/pkgconfig` 供 mlua 系统模式调用
6. 报告 `luajit -v`（应显示 LuaJIT 2.1.0-beta3 / current stable head）

mlua dev-dep 使用 `vendored` feature 自带 LuaJIT，因此**该脚本对当前 bench crate
不是必需**；保留为日后切换到 system LuaJIT 时使用（如果想测对照 system / vendored
之间的 trampoline 差异）。

### 2.2 `crates/relon-bench/Cargo.toml` — mlua dev-dep

```toml
[dev-dependencies]
mlua = { version = "0.10", features = ["luajit", "vendored", "macros"] }
```

`vendored` 让 mlua 自己 build LuaJIT，避免依赖 system install。如果 vendored
构建失败：fall back 顺序 = `luajit52` → `lua54`（vanilla Lua，slower 但稳）。

### 2.3 `lua_boundary_calibrate` bench 行

加在 `crates/relon-bench/benches/trace_jit_hot_loop.rs`：

- 行 ID：`lua_boundary_calibrate`
- Workload：mlua 创建 Lua state，注册一个 `function noop() return 42 end`，
  Rust 侧循环 `HOT_LOOP_N = 1_000_000` 次调用。
- 用途：测 mlua → Lua boundary 自身的 cost（无 Lua 工作量），作为后续 λ-2
  paired workloads 的基线（要从 W1-W12 数据里减除）。
- 预期：50-200 ns/iter（比 Relon dispatch 慢，Lua state 设置非 trivial）。

### 2.4 `crates/relon-bench/tests/lua_smoke.rs`

- `lua_one_plus_one_is_two`：mlua/LuaJIT 跑 `return 1 + 1`，断言 `Value::Integer(2)`。
- `lua_sum_loop_returns_expected`：100 iter sum loop，断言 4950。
- `lua_boundary_cost_in_ballpark`：标 `#[ignore]`，测 boundary cost 是否在
  50-200 ns 区间。

---

## 3. Gate / 验证

| Gate | 结果 |
|---|---|
| `cargo build --workspace` | OK（45 s 冷编） |
| `cargo test --workspace` | **1798 passed, 0 failed, 5 ignored**（比 λ-0 基线 1793 多 5：3 个 quiescence 单测 + 2 个 Lua smoke） |
| `cargo clippy --workspace --all-targets -- -D warnings` | OK（修了 result_large_err + manual_range_contains） |
| `cargo fmt --all -- --check` | OK |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | OK（21 s） |
| `cargo bench --bench trace_jit_hot_loop` | OK（force-run on dev box；新行 captured，见 §4） |
| `cargo test -p relon-bench --test methodology_validators` | **9 passed**（验证 ≥ 12 closures 含新 lua_boundary_calibrate） |
| `cargo test -p relon-bench --test lua_smoke` | **2 passed, 1 ignored**（boundary cost ballpark `#[ignore]`d） |

---

## 4. 实测 lua_boundary_calibrate 单行（dev-box，未 quiescent）

机器状态：governor=schedutil（非 performance）、no_turbo=1、load1=4.47、
`RELON_BENCH_FORCE_RUN=1` 强跑。**这个数字仅用作 wiring 验证 + 量级 sanity-check**；
λ-2 跑前必须在 quiescent 机上重测。

| Row | p50 | p99 | max | samples | tag |
|---|---|---|---|---|---|
| `lua_boundary_calibrate` | **94.90 ns/iter** | 94.92 | 94.92 | 2（quick 模式） | per_iter_alloc |

**关键观察**：
- 94.9 ns/iter 在 task brief 预期的 50-200 ns 区间内（mlua → LuaJIT pcall 边界，
  含 Lua state stack-balance check + lua_pcall longjmp anchor）。
- 比 Relon dispatch rows（9.5 ns）慢 ~10×，正如 plan §3 W8 推测的 "Lua state
  设置比 Relon trampoline 重得多"。
- 这就是 λ-2 paired workloads 用来 normalize Lua-side numbers 的减除基线。

**Caveat**：本次跑是 `--quick`（2 samples），分布尾部不可靠。完整 200-sample
distribution 要在 quiescent 机上重跑：
```bash
./scripts/bench_quiescence.sh
taskset -c 4-7 cargo bench --bench trace_jit_hot_loop
cargo run --release -p relon-bench --bin bench_stats -- \
    target/criterion/v6_epsilon_hot_loop
```

---

## 5. Carry-over

- **λ-2**（12 workload paired bench）：W1-W12 各写 Relon 版 + Lua 版，
  跨 backend × Lua 共 50 个 measurement。boundary cost 用本阶段 calibrate
  数据从所有 Lua row 里减除。
- **λ-3**（跑全 50 + 分析）：4-way Relon backend × LuaJIT 出 ratio 表 + 8
  维度归因。
- **本阶段 follow-up**：如果用户机 governor 不可锁，必须由 `RELON_BENCH_FORCE_RUN`
  bypass，并在 λ-3 报告里说明这次 bench 是在 unquiet 机上跑的。

---

## 6. 文件清单（incremental，agent commit 后填入）

新增：
- `scripts/bench_quiescence.sh`
- `scripts/install_luajit_2_1.sh`
- `crates/relon-bench/src/quiescence.rs`
- `crates/relon-bench/tests/quiescence_check.rs`
- `crates/relon-bench/tests/lua_smoke.rs`

修改：
- `crates/relon-bench/Cargo.toml`（加 mlua dev-dep、quiescence 模块 export）
- `crates/relon-bench/src/lib.rs`（pub mod quiescence）
- `crates/relon-bench/benches/trace_jit_hot_loop.rs`（加 lua_boundary_calibrate 行 + quiescence 自检 panic）
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§22 追加）
- `docs/internal/wasm-bench-report-2026-05-16.md`（lua_boundary_calibrate 单行追加）

不曾 push。
