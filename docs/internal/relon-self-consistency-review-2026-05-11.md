# Relon 目标与实现自洽性批判记录（2026-05-11）

> 上一轮见 [`relon-self-consistency-review-2026-05-10.md`](./relon-self-consistency-review-2026-05-10.md)。
> 本轮基于 `main @ 8978ffc`，schema-rooted Phase A.1 / B / C / D 全部落地后的状态。

## 结论

核心语言与工具链主线**大体自洽**：三层架构、schema-rooted dispatch、constraint witness lowering、6 bit capability 模型都按设计文档落地；855 项测试全绿。

但**对外承诺（默认沙箱、确定性、quickstart、英文文档）与可验证状态之间存在多处不易察觉的缺口**，部分缺口比上一轮留下的清单更靠前。最值得优先处理的不是再加语言特性，而是**把"对外门面 + 沙箱实测语义"补到与核心代码一致**。

## 自洽且达到承诺的地方

- 三层架构 `parser` / `analyzer` / `evaluator` 责任划清，新加的 schema-rooted dispatch、`with { ... }`、`#extend`、constraint witness、Indexable / Iterable lowering 都按 §J 设计文档逐项落地（`crates/relon-analyzer/src/{core_schemas,extend,constraints}.rs`、`crates/relon-evaluator/src/eval.rs:1752` 起的 `try_call_schema_method`）。
- 855 个测试全绿（`cargo test`），与 `roadmap.md` 自述吻合。
- capability bit 从单 bit `reads_fs` 扩到 6 bit、`Capabilities` 与 `NativeFnGate` 都打了 `#[non_exhaustive]`（`crates/relon-evaluator/src/eval.rs:26 / :96`），analyzer 侧 mirror 完整。上一轮报告的 P0 这条确实闭环了。
- stdlib 纯度有 `purity_guard` 编译期测试守门（`crates/relon-evaluator/src/stdlib.rs:1215`），扫禁字符表覆盖 fs / env / net / process / time / rand / chrono / tokio / reqwest。

## 不自洽 / 与对外承诺不一致的硬缺口

### 1. README 头版例子直接跑不了

README "Example" 段的代码：

```relon
{ @fn(val, symbol) "currency": val + " " + symbol, ... }
```

本地执行报 `Variable not found: val`：`@fn(...)` 装饰器并不绑参，正确写法是 `currency(val, symbol): ...`（见 `examples/demo.relon`）。这是项目首屏的认知陷阱。

### 2. Quickstart 命令本身在主分支上失败

README 给的 `cargo run -p relon-cli -- run examples/demo.relon` 在干净仓库会直接挂掉：

```
error: `cargo run` could not determine which binary to run.
available binaries: bench, relon-cli
```

原因是 `crates/relon-cli/src/bin/bench.rs` 与 `src/main.rs` 同包暴露成两个 bin，没有 `default-run`。把 bench 放进 `examples/` 或加 `default-run = "relon-cli"`，或 README 改成 `--bin relon-cli`，三选一。

### 3. clippy "ship gate" 在当前工具链上本身就过不去（已治标）

README 把 `cargo clippy --workspace --all-targets -- -D warnings` 作为发布前必跑指令，但本机 `rustc 1.93` 上跑出 **165 个 warning**：

```
warning: value assigned to `range` is never read (×38)
warning: value assigned to `name`  is never read (×9)
... 共 165 条
```

根因不是 thiserror / miette，而是 rustc `unused_assignments` lint 的 stable→stable 回归（`rust-lang/rust#147648`，cjgillot：*"we should not emit lints inside a proc-macro code, so this is a bug"*）。所有用 proc-macro derive 的 crate 都受影响（miette / thiserror / displaydoc / zeroize / relm4 都在 issue 串里被点过名）。

**当前处置**：
- 把 `thiserror` 升到 `2`、通过 `[workspace.dependencies]` 收口（独立价值的版本现代化，不为修这个 lint）。
- `relon-analyzer/src/lib.rs` + `relon-evaluator/src/lib.rs` 各加 `#![allow(unused_assignments)]` + 指向 `rust-lang/rust#147648` 的注释。
- 等上游 rustc 修了就把这条 allow 删掉。

### 4. ~~"默认沙箱" 的 `max_value_elements` 在 stdlib 路径上是漏的~~（已修）

`Capabilities::max_value_elements` 的 sandbox 文档承诺过："字面量、`+` 合并、推导式都会检查"。原始版本下 `check_value_size` 调用点只有 4 处（`eval.rs:855 / :950 / :992`，`arithmetic.rs:112`），**所有 stdlib intrinsic 返回的集合都不过这道闸**：

- `range(0, 1_000_000)` 直接在 `Range::call` 里 `Value::list(...)` 收集千万元素（`stdlib.rs:333`）。
- `list.map` / `list.filter` / `list.reduce` / `string.split` / `string.replace` / `dict.merge`（除字面量 spread 的那一个路径外）类似。
- 实测：`{ x: len(range(0, 1_000_000)) }` 在沙箱 default Context 下正常返回 `1000000`；即使 `max_value_elements = Some(3)`，因为 `len(range(...))` 的中间结果不进 `Expr::List`、不进推导式、不进 dict-merge，cap 永远不会触发。

修法落地：

1. `NativeFnCaps` 多了一个 `max_value_elements()` 方法（`crates/relon-evaluator/src/native_fn.rs`），把 cap 暴露给 native fn 自查；`EvaluatorCaps` 读 `Capabilities::max_value_elements`。
2. `Range::call` 在分配 `Vec<Value>` 之前对 `end - start` 与 cap 做预检——`range(0, 10_000_000_000)` 立即被拒绝，不会 OOM。
3. `Evaluator::call_function` / `Evaluator::try_call_native_method` 在拿到 native fn 返回值后统一走 `check_value_size`，覆盖所有自由函数 + 接收者方法 + host 注册的 native fn。`check_value_size` 仍只看最外层容器（递归大小检查是独立话题）。
4. `docs/zh/guide/sandbox.md` 的 `max_value_elements` 段落已重写，删除 "host fn 返回值是宿主自己的问题" 的豁免；现在 stdlib 走和字面量同一道闸，host 真要无约束请显式设 `max_value_elements = None`。
5. `sandbox_tests.rs` 新增 6 条 regression：`range` 预检、`_string_split` / `_list_map` / `_list_filter` / `_dict_merge` 函数式、`xs.map(...)` 接收者式（搭 `with_analyzed`）、和一条 at-cap 正向用例。

### 5. "无 implicit ambient state / 确定性" 与全局 Iter cursor 表的张力

`stdlib.rs:1068` 的 `iter_cursors()` 是 `&'static OnceLock<Mutex<HashMap<u64, usize>>>`，`next_iter_id()` 是同一进程的 `AtomicU64`。后果：

- 同一进程内并发跑多个 `Context` 时，cursor 表共享、id 计数器共享。这并不破坏单条 iter 的语义，但和 README "no implicit ambient state … no iteration-order leaks" 的语境产生认知摩擦——多租户嵌入下，cursor 表里的条目会一直累积。
- roadmap 把它记为 "16 B leak"，但实际问题不是 16 B，而是**租户隔离失败**：宿主无法在 evaluate 结束后清理某次运行产生的 cursor entries（Context 拿不到 `_id` 的清单）。
- 修法：cursor 表挂到 `Context` 上（即 roadmap 那条 "NativeFnCaps trait 扩展"），并在 `eval_root` / `run_main` 结束时清空，和 `path_cache` 同处理。

### 6. README "There is no trusted mode" 与 `--trust` / `Capabilities::all_granted()` 的措辞冲突

英文 README 里的原话是 *"There is no 'trusted mode' the script can fall back to."* 但 `crates/relon-cli/src/main.rs:46` 有 `--trust`、`crates/relon-evaluator/src/eval.rs:66` 有 `all_granted()`、`crates/relon-evaluator/src/module.rs` 有 `FilesystemModuleResolver::trusted()`。

严格读其实没说谎——"the script" 不能自提权，trust 是 host 的开关。但是字面上看就是矛盾。建议改成 *"scripts can't elevate themselves; the host can choose to grant all caps explicitly via `--trust` / `Capabilities::all_granted()`"*。否则审计员看 README 会以为整套 `--trust` 路径不存在。

### 7. 英文文档承诺远超实际

- README 只在 spec 一处挂了 `· English` 链接（目标 `docs/en/guide/spec.md` 存在）；use-cases / architecture 段虽然没写 English 链接，但 vitepress 站点给读者的预期是双语对照。
- vitepress 配置（`docs/.vitepress/config.mts`）的英文 sidebar 只有 2 项（introduction + spec），中文有 11 项；use-cases / architecture / host-integration / sandbox / types / modules / stdlib / syntax / functions 九条全都没有英文版。
- 上一轮 review 已经点过"英文文档明显弱于中文"，至今没动。

### 8. ~~仓库身份信息不一致~~（已统一为 `kookyleo/relon`）

历史状态：`Cargo.toml:11` 写 `relonlang/relon`，`docs/.vitepress/config.mts:108` + 两个 `index.md` + 英文 `introduction.md` 都是 `kookyleo/relon`，五处中四处一致、一处孤儿。现已把 `Cargo.toml` 的 `repository` 字段统一到 `kookyleo/relon`，作为 canonical URL。后续发布到 crates.io 不会因为 repo 校验失败被拒。

### 9. 文档/代码状态漂移的细节

- `crates/relon-analyzer/src/constraints.rs:106-118` 还写着 "lowering ... is the still-pending hook"，但 roadmap §J 已经把 Iterable / Indexable / 算子 lowering 都标 `[x]`。代码内注释和路线图不同步。
- `roadmap.md:160-176` 列了四条"剩余未决项"（中段 path 推断落 Any、method generic K/V 仅 wildcard、shadow warning、Iter cursor leak），其中 Iter cursor leak 见上文已经从"节省 16 B"升级到"影响多租户隔离"，应被重新分类为安全/正确性而不是 housekeeping。

### ~~10. 多 binary 的杂物外溢~~

~~`relon-cli` 包内同时挂着面向用户的 CLI 和内部用的 `bench` benchmark。`bench` 用 `Instant::now()`、`Duration`，本身没问题，但暴露成 `relon-cli` 包的二级 bin 让 `cargo run -p relon-cli` 这条最常用命令直接失败（见 #2）。`bench` 应该挪到 `crates/relon-cli/examples/` 或者一个独立的 `relon-bench` 包 / `#[cfg(feature = "bench")]`，让用户面 CLI 保持单 bin。~~

（已闭环 2026-05-11：`crates/relon-cli/src/bin/bench.rs` 已抽出为独立的 `crates/relon-bench` 包，并标记 `publish = false`；作为跟进清理，`relon-cli/Cargo.toml` 中的 `default-run = "relon-cli"` 已删除——每个包恰好一个 bin 后 `cargo run -p relon-cli` 无需 default 声明即可消歧。）

### ~~11. `register_fn` API 收口仍留半口~~

~~`roadmap.md:35-44` 写的"把 `register_fn` / `register_fn_with_caps` 合并为单 `register_fn(name, gate, fn)`"已落地，但 `allow_native_fn` HashSet 还在（`crates/relon-evaluator/src/eval.rs:31-32`）——它是一个**绕过 per-bit gate 的纯名字白名单**，文档 `sandbox.md:165-179` 解释为 "我就是想放某一个函数过"。这条小后门和六位能力模型并存，会让 host 集成的最小授权失败模式难以审计——"为什么我把所有 bit 都关了它还能跑？因为有人写过 `allow_native_fn.insert("...")`"。短期是文档强调，长期建议把 `allow_native_fn` 改名为 `force_allow_native_fn` 或者要求只有 trusted Context 才能写入。~~

（已闭环 2026-05-11：`allow_native_fn` / `allow_all_native_fn` 字段已从 evaluator 与 analyzer 镜像中删除；`Capabilities` 仅保留 6 个能力 bit + 2 个预算，授权路径单一。`Capabilities::all_granted()` 改为直接翻 6 个 bit，原"all-granted"语义在 per-bit gate 下行为等价。）

## 其他偏向中等优先级的观察

- `max_steps` 的实测语义是 "AST 节点 dispatch 次数"，对 stdlib 大输入的内循环（`range`、`list.reduce` 在百万元素上的 close walk）一步只 +1，**对最长尾的 DOS 表面并不严密**。需要 native fn 内部主动 yield / 自计步，或者 stdlib 大循环里也调 `step_counter.fetch_add`。
- `core/iter.relon` 等四个内置 schema 都是 `include_str!` 嵌进 analyzer，但是 evaluator 的 `register_pure_method` 表是手写的 17 条镜像（`stdlib.rs:102-142`）；两边只有 "decision 21'" 的注释约束、没有编译期/测试期对照（"core/*.relon 声明的每条 `#native` 都必须有对应 register_pure_method"）。建议加一条 `#[cfg(test)]` 交叉断言。
- examples 只有两个 `.relon` 文件，对一个想做"开箱即用业务规则 DSL"的项目偏薄。`use-cases.md` 提到了 feature flag、pricing、workflow，但 `examples/` 里没有对应玩具，新用户读不到落地形态。
- workspace `Cargo.toml` 把 `winnow = "0.6"` / `memchr = "2.7"` 放到 `[workspace.dependencies]`，但 `relon` facade、`relon-cli` 各自有 `miette` / `serde_json` / `clap` 版本声明而不走 workspace deps。版本散落以后会出现 multi-version。

## 建议的优先级

| 优先级 | 项 | 行动 |
| --- | --- | --- |
| ~~P0~~ | ~~stdlib intrinsic 返回值不受 `max_value_elements` 约束~~ | ~~已修：`Range::call` 在分配前对 `caps.max_value_elements` 做预检；`call_function` / `try_call_native_method` 在 native fn 返回后统一走 `check_value_size`；`sandbox_tests` 新增 range / `_string_split` / `_list_map` / `_list_filter` / `_dict_merge` / 接收者方法路径 6 条 regression test~~ |
| ~~P0~~ | ~~README quickstart 跑不动 + 头版示例不能 eval~~ | ~~已修：`relon-cli/Cargo.toml` 加 `default-run = "relon-cli"`；README 头版例子改成可运行的方法简写形式；"unified closures (`@fn`)" 改成实际语法~~（后续治本：见 §10——`bench` 抽到独立 `relon-bench` 包，`default-run` 已撤回）|
| ~~P0~~ | ~~`clippy -D warnings` 在文档承诺的命令下失败~~ | ~~已治标：thiserror 升 2 + 两 crate 加 `#![allow(unused_assignments)]` 引用 rust-lang/rust#147648；等上游 rustc 修后删 allow~~ |
| P1 | 多租户 Iter cursor 隔离 | cursor 表挂到 `Context`，`eval_root` / `run_main` 末尾清空 |
| P1 | 仓库 URL 漂移 + 英文文档承诺空洞 | 选一个 canonical repo url 全量替换；要么把英文文档补到与中文对齐，要么 README 不要再写 "· English" |
| ~~P1~~ | ~~`allow_native_fn` 与 6-bit 能力的语义重叠~~ | ~~文档/命名上明确为"强制覆盖通道"，加 audit log~~（已闭环 2026-05-11：两字段已删除，授权路径收口为单一 6-bit gate 检查） |
| P2 | constraints.rs 注释与 roadmap 状态漂移 | 清理 "still-pending hook" 注释；roadmap 上把 Iter cursor leak 重新分类 |
| P2 | core schema vs stdlib mirror 的双源 | 加 `#[cfg(test)]` 交叉断言 |
| P2 | examples 过薄 | 至少补 feature flag / pricing / workflow 三个落地玩具 |

## 一句话定性

语言核心和 schema-rooted dispatch 这条主线已经基本闭环；**但项目对外的安全承诺与工程门面（沙箱默认、quickstart、clippy gate、英文文档）落后于核心代码两到三步**——这些缺口比再加一条语言特性更值得先补。
