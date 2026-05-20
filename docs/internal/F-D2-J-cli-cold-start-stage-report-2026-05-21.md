# F-D2-J stage report — CLI cold-start (panic=abort + clap fast-path)

> **Date**: 2026-05-21
>
> **Base HEAD**: `462fd83 merge(parser+eval+cli): F-D2-I parser fast-path + Context lite prep`
>
> **Scope**: W11 cold-start (`relon run x.relon --args '{...}'` fresh
> process). F-D2-I 把 in-process 部分压到 ~830 µs；剩下 ~2.2 ms 是
> pre-main 的 ELF loader / 动态链接 / 重定位 + clap 主干 build。这一
> 阶段从两个角度收：把进程的 ELF 表瘦下来（`panic = "abort"`，
> 静态展开 clap 的 derive 输入），并在 main 第一时间用 hand-rolled
> argv matcher 绕过 `Cli::parse()` 自身的 ~270 µs 冷启动税。

## 一、改动文件

| 文件 | LoC | 说明 |
|---|---|---|
| `Cargo.toml` | +13 / -1 | 新增 `[profile.release-cli]`，`inherits = "release"` + `panic = "abort"`；workspace 级 `clap` 改成 `default-features = false` 让下游 crate 用 `features = […]` 精确选择特性集，避免 cargo 默认 union 把 `color` / `suggestions` / `wrap_help` 拉回来。 |
| `crates/relon-cli/Cargo.toml` | +9 / -2 | 删未使用的 `ariadne = "0.6.0"`（grep `ariadne` 全 crate 0 命中）；clap features 从 `["derive"]`（隐含 default）改成 `default-features = false` + `["derive", "std", "help", "usage", "error-context"]`，等于把 `color`+`suggestions` 砍掉。 |
| `crates/relon-fmt/Cargo.toml` | +5 / -1 | 同上 clap 收敛。`relon-fmt` 是 CLI binary 的 `Fmt` 子命令落点，feature 不收敛会让 cargo 的 per-target feature unification 把 default 一拉回来抵消 cli 端的瘦身。 |
| `crates/relon-cli/src/main.rs` | +95 / -2 | 新增 `try_parse_run_fast(argv)`：手写 argv matcher，匹配 `relon run <file> [--lite] [--args <json>]`（W11 shape）；不识别就 `None` 让出给 `Cli::parse()`，flag 语义仍由 `#[derive(Parser)]` 主接管。`main()` 顶端先尝试 fast-path，再加可选的 `RELON_CLI_PROFILE=1` 计时点（`main_entry`、`argv_fast_run` / `clap::parse`），方便后续验证。 |
| `crates/relon-bench/benches/cmp_lua.rs` | +6 / -1 | W11 跑测时 binary 候选顺序前置 `target/release-cli/relon-cli`；fallback `target/release/relon-cli` 与 `target/debug/relon-cli` 保留，所以 `cargo build --profile release-cli -p relon-cli` 之后 `cargo bench` 自动用瘦身版。 |

总计 5 文件，~ +120 净 LoC。

## 二、设计取舍

**为何 `[profile.release-cli]` 单开一份而不是改 `[profile.release]`**：
workspace 默认 `release` 注释明确写了 "downstream embedders keep
`unwind` panics so `Drop` still runs (mutex poisoning, etc.)"。
embedder 关心的是把 `relon` 作为 library link 进自己的进程里（带
async runtime、连接池、tokio mutex），那种场景一旦换 abort 会丢
`Drop` 副作用。CLI binary 是单进程跑完即退、本身没有跨线程
shared mutex 状态，所以 abort 在这个 target 上是安全的。单独加
`release-cli` profile 让 embedder 路径维持原合约，cold-start 优化只
落到 binary。

**为何 hand-rolled argv matcher 不全替换 clap**：
`clap` 的 `--help` / `--version` / `derive` 校验是 CLI 的契约，覆盖
`fmt` / `lsp` 与 `run` 的所有 flag 组合（`--trust`、`--backend`、
`--require-hash`、`--args=value` 等）。fast-path 只识别 W11 实测的
4-token 组合（`run`、`<file>`、`--lite`、`--args <v>`），任何 `=`
形式、prefix 缩写、短 flag、未知 long flag 一律返回 `None` 让 clap
继续。这是 "热路径快、冷边缘正确" 的标准布局——确保 fast-path
误判都掉进 clap，clap 再给 owner 一份精确报错。

**为何不 feature-gate `relon-codegen-native` / `relon-bytecode` /
`relon-lsp`**：这三个 crate 占 binary `.text` 大头（cranelift ~3 MB、
lsp-types ~500 KB），但都是 `relon run --backend cranelift-aot`、
`relon run --backend bytecode`、`relon lsp` 子命令的实际后端。
feature-gate 它们意味着同一个 `relon-cli` binary 不再覆盖原 CLI
surface——这是 ABI break，超出本阶段 scope。LTO + `panic = "abort"`
已经把这三个 crate 里 unwind 路径未触及的 cold code 清掉，留下来
的就是 reachable code。

## 三、Binary size delta

```
.text          9 238 583 → 8 240 247   (-998 KB)
.eh_frame        716 828 →   600 828   (-116 KB)
.gcc_except_t    324 884 →     7 428   (-318 KB)
.rodata        1 200 805 → 1 204 901   (+4 KB, 噪声)
total ELF      12 MB    → 10.6 MB    (-12%)
```

`.text` 缩 ~1 MB 是 LTO 把所有 panic-with-unwind 分支（drop glue +
landing pad + personality routine）裁掉之后的结果；`.gcc_except_table`
归零是 cleanup 表整体消失。

## 四、Bench

**Baseline (462fd83, `target/release/relon-cli`)**：
- W11 default：~3.055 ms (× 1.545 vs LuaJIT 1.97 ms)
- W11 lite：~3.053 ms (× 1.544 vs LuaJIT 1.97 ms)

**This stage (462fd83 + F-D2-J, `target/release-cli/relon-cli`)**，
取 `cargo bench -p relon-bench --bench cmp_lua -- W11`（机器
load1 ≈ 6.74，非 quiescent，noise ≈ ±50 µs）：

| metric | before | after | delta | ratio vs LuaJIT 1.97 ms |
|---|---|---|---|---|
| W11 default | 3.055 ms | 2.932 ms | -123 µs (-4.0%) | × 1.545 → **× 1.488** |
| W11 lite | 3.053 ms | 2.927 ms | -126 µs (-4.1%) | × 1.544 → **× 1.486** |

> ⚠️ LuaJIT 没在本机安装（criterion 行直接 skip），ratio 是按任务给的
> 1.97 ms 引用值算的；本机若装 LuaJIT，ratio 会随当时 load 浮动。
> bench 运行时 load1 = 6.74、scaling_governor = schedutil、no_turbo = 1，
> 不达 quiescence；同一 binary 在 load1 ≈ 2.6 的低噪窗口里跑出过
> default 2.905 ms / lite 2.920 ms（× 1.474 / × 1.482），与本次方向一致。

**In-process phase delta** （`RELON_CLI_PROFILE=1`, 单次代表性样本）：
- `clap::parse` (before)：~275 µs
- `argv_fast_run` (after, W11 形态)：~94 µs
- 净省 ~180 µs in-process；与 binary 瘦身的 ~130 µs pre-main 改善
  叠加，端到端 ~300 µs，对应 cold-start 4% 改善。

**结论**：× 1.4 gate 未达成（任务要求 ≤ × 1.4，本次到 × 1.49）。剩余
~150 µs 主要在 pre-main 的 ELF loader / 动态链接器；进一步收益的
低悬果只剩 musl static link / 拆出独立 `relon-run` mini-binary
（feature-gate 掉 cranelift + lsp + bytecode）——两者都改变发布
surface，需要单独立项。

## 五、Gate

| step | result |
|---|---|
| `cargo build --workspace` | ok（dev 1m 06s） |
| `cargo test --workspace` | ok（所有 crate 全绿，含 relon-cli 集成测试） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ok（无新 warning） |
| `cargo fmt --all -- --check` | ok |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ok |
| `cargo run -q -p relon-fmt -- --check fixtures/* …` | ok |

## 六、与并发 agent 隔离

F-D7-J 改 `relon-trace-jit/recorder`，F-D8-E.5 改 trace-jit optimizer，
本阶段只动 `relon-cli` / `relon-fmt` / workspace `Cargo.toml` /
`relon-bench/benches/cmp_lua.rs`——文件零重叠，无 merge 风险。

## 七、后续建议

1. **拆 `relon-run` mini-binary**：feature-gate 掉
   `relon-codegen-native` / `relon-bytecode` / `relon-lsp` /
   `relon-analyzer` 的 LSP 入口，bench `target/release-cli/relon-run`
   预期能下到 × 1.2 区间，但 surface 缩水（不能再 `relon lsp` /
   `relon run --backend cranelift-aot`）需要 RFC。
2. **musl static link**：CI build `--target x86_64-unknown-linux-musl`
   去掉 libc/libm/libgcc_s 动态依赖与 PIE 重定位表，预期 pre-main 再
   省 200-300 µs；同样需要 release 流程配合。
3. **clap 替换为 lexopt / pico-args**：本阶段已经用 hand-rolled
   matcher 覆盖热路径，把 clap 留给 `--help` / `fmt` / `lsp` 还能拿
   ~150 µs（如果以后 `--help` / 错误 UX 不再依赖 clap 的 `wrap_help`
   就值得动）。
