# wasm-AOT v4 路线图（sandbox-safe，2026-05-18）

> 目标：warm invoke 7-12 μs → **1-3 μs**，摸到 PUC Lua interpreter 上限。
> 约束：**不动 wasmtime sandbox**（linear memory bounds + trap handler + capability gating + spectre mitigation 全保留）。
> 范围：v3++ 七项（b-1 ~ b-7）落地后启动；与 v3++ 不冲突。

## 现状基线（main HEAD 截至 b-6-tail / 待补 b-7）

| 路径 | wasm-AOT | tree-walk | LuaJIT 典型 | PUC Lua 典型 |
| --- | ---: | ---: | ---: | ---: |
| warm invoke (`stdlib_title` 60-cp mixed) | 7.18 μs | 4.72 μs | < 1 μs | 1-5 μs |
| warm invoke (`stdlib_upper` 100B ASCII) | 12.38 μs | 3.24 μs | < 1 μs | 1-5 μs |
| cached cold start | 188.77 μs | 1.10 ms | n/a | n/a |

warm invoke 的 ~7-12 μs 分解（估算）：

| 组件 | 占用 |
| --- | ---: |
| wasmtime Store reset + linear memory zero | 4-5 μs |
| 参数 encoding 入 linear memory | 1-2 μs |
| `TypedFunc::call` dispatch | 0.5-1 μs |
| wasm body 业务 op | 1-3 μs |
| 返回 decoding | 0.5-1 μs |

## 阶段安排（按 ROI 排序）

### v4-a：Pool-of-Stores **dirty-leave**

**改动**：当前 Pool-of-Stores（Phase 9.b-1）的 store 复用还按 wasmtime 默认行为 zero 整段 linear memory。给 evaluator 加 `RegionMap`：跟踪每次 invoke 实际碰过哪些 page（输入区 + 输出区 + bump cursor），其它 page leave dirty。下次 invoke 业务逻辑覆盖写自己的 scratch，无需清零。

**ROI**：warm invoke **-4 μs**（约 60% 节省 store reset 成本）。

**工程量**：中（3-5 天）。要细审 wasm body 哪些 page 必须 zero（如 bump cursor 起点必须 0、IO 头部 length prefix 必须重写）。需要给 codegen 加 "scratch-clobber" 标记 + evaluator 侧脏 page 跟踪。

**风险**：测试覆盖关键 —— 如果忘 zero 某 page 会污染下次 invoke 输出。

### v4-b：参数 bridge specialization

**改动**：当前 `write_value_into_builder`（evaluator.rs 入口）是 runtime type-dispatch per field。AOT 期已知 `#main(...)` 签名，codegen 出一个 per-shape encoder：给定 `main_schema`，生成专门的 wasm 函数 `__pack_args_<hash>(host_ptr, scratch_ptr)`，host 侧 Rust 出对应 memcpy / `to_le_bytes` 平铺逻辑。每次 invoke 直接 memcpy 而非 walk `Value` 树。

**ROI**：warm invoke **-1.5 μs**。

**工程量**：小（2-3 天）。`SchemaLayout::offsets_for` 已经把字段 offset 算出来了，只是 runtime 还在用。

### v4-c：stdlib helper 内联宏化

**改动**：`__casefold_lookup` / `__compose_pair` / `__nfd_lookup` / `__range_lookup` 等当前是 wasm function（prologue/epilogue cost）。改 codegen-wasm：callsite 直接 emit binary-search 字节码 inline，不再 `call`。可以用 op count 阈值（≤ 30 op 的 helper inline）。

**ROI**：warm invoke **-1 μs**；wasm 模块字节数 **-20%**（消除 helper 函数 prologue/epilogue + table import 重复声明）。

**工程量**：小（2 天）。已有 `stdlib_function_index` 表，加 inline 模式参数即可。

### v4-d：mmap AOT cache + lazy code page-in

**改动**：`wasmtime::Module::deserialize_file` 已经支持 mmap path。换 `AotCache::load` 实现用 mmap。同时把 native code section + data section 在 .meta 里拆开记录 offset，让 OS 按需 page-in code（首个 trap / first call 触发）。

**ROI**：cached cold start **190 μs → 80 μs**（-60%）。

**工程量**：极小（1 天）。

### v4-e：auto-tier backend

**改动**：当前 `--backend tree-walk` / `--backend wasm-aot` 是 CLI 显式选择。加自动选择：IR op 数 < 阈值 → tree-walk，≥ 阈值 → wasm-AOT。阈值实测定（约 50-100 op）。SDK `relon::new_evaluator(source)` 默认走 auto。

**ROI**：中小程序（典型 config 文件）warm 路径 **-50%**（直接拿 2.7 μs tree-walk）+ cold start wasm cost → 0。大程序 wasm-AOT 仍然胜出。

**工程量**：小（与 b-1 user-fn DCE 复用 IR op 计数器）。独立于 v4-a/b/c，**可以提前做** —— 立刻给中小用户 2-3× 透明加速。

### v4-f：SIMD ASCII fast-path（即现 v3++ b-7）

**改动**：wasm v128 SIMD 一次 16 字节 ASCII 检测 + casefold。Greek / Hangul / 组合标记走原慢路径。`upper` / `lower` / `title` 三个 stdlib 受益。

**ROI**：ASCII-heavy 串 warm **-50%**；模块字节数 **+15%**（SIMD 字节码偏长）。

**工程量**：中（4-5 天）。`wasm-encoder` 0.249 已支持 `Op::V128Load / V128Store / I8x16` 等指令。

### 附加细调（不单独立 phase）

- **`Config::wasm_backtrace(false) + native_unwind_info(false)`**：关 Rust 侧 backtrace 收集，trap 仍正常工作。-200 ns/invoke。
- **AOT 时把 op 计数估算 fuel 写进 `.meta`**：fuel 从 per-invoke `set_fuel` 转 epoch interruption 一次性 deadline。-500 ns。
- **fuel feature default-off**：fuel 是 DoS 保护，不是 sandbox。host 已有 outer-loop deadline 时关 fuel 直接 -500 ns/invoke。CLI flag `--fuel-limit 0` 改成 unset 默认。

## 累计估算

trivial arith 场景（最 sensitive，当前 7 μs）：

| 阶段叠加 | warm invoke |
| --- | ---: |
| 当前 (post-v3++ b-7) | 7 μs |
| +v4-a | ~3 μs |
| +v4-a/b | ~1.5 μs |
| +v4-a/b/c | **0.5-1 μs** |

criterion 噪声底 ±200 ns，1 μs 以下测不动。

cached cold start：

| 阶段叠加 | cold start |
| --- | ---: |
| 当前 | 190 μs |
| +v4-d | **80 μs** |

## 物理上限

LuaJIT sub-μs warm 来自 trace JIT 出 native code，函数调用 = `call rax`。Relon 走 wasmtime 哪怕做到极限：**v4 a-f 全做完，warm 大约 1-3 μs，摸到 PUC Lua interpreter 上限**。要继续往 LuaJIT 那条线压，物理上得：

- 换 runtime（Wasmer headless mode / 自己包 cranelift 出的 .o + 自己实现 trap handler）
- 或者放弃 wasm 作中间表示，Relon IR 直出 cranelift IR + 自己实现 linear memory bound check

两条都**动 sandbox**，明确不在 v4 范围。落到 v5 系列推（用户已授权）。

## v5 系列：放下 wasmtime / wasm，物理逼近 LuaJIT

**用户授权**：2026-05-18 明确 "wasmtime 包括 wasm 都不用守，看需要" + "但沙箱是需要的，恰当的形式"。

**硬约束**：sandbox 语义**必须**保留，但**实现形式不限于 wasmtime + wasm spec**。等价语义可由 cranelift bounds check + Rust signal handler + capability bitmap 等组合提供。具体来说必须有：

1. Linear memory bounds check（每次内存访问越界 trap）
2. Trap handler（除零、未定义、bounds violation 等捕获并安全退出，不破坏 host 进程）
3. Capability gating（受控的 host fn 调用 + 远程 import 闸门）
4. Resource limit（fuel / epoch / deadline，可缩减但不能完全去）

放弃下列保留是允许的（按需）：
- spectre mitigation（hardware speculative bounds check）—— 关闭可 -100 ns/loop iter
- Rust backtrace / unwind info emission —— 关闭可 -200 ns/invoke
- 完整 wasm spec compliance —— 不需要外部 wasm runtime 加载我们的输出
- 标准 wasmtime 抽象（Store / Instance / Func）—— 可重写

v5 在 v4 之上启动，按 ROI 取舍。

### v5-α：Wasmer headless mode

**改动**：用 Wasmer 的预编译模式替换 wasmtime —— compile-time 把 wasm 模块编译成静态链接的 native code + 最小 runtime，省 wasmtime 的 per-invoke `Store::new` / `Instance::new` 重逻辑。仍走 wasm spec，linear memory bounds 仍有，但 trap handler / fuel / spectre mitigation 走 Wasmer 简化实现。

**ROI**：warm invoke **-1 ~ -2 μs**（绕过 wasmtime store 抽象，直接调 native code）。

**工程量**：M（替换 codegen-wasm 的 runtime 部分；Wasmer + wasmtime API 不完全 compat，要适配 `Module::serialize` / `Func::call` / capability import wiring）。

**风险**：Wasmer 生态弱于 wasmtime，未来升级跟随成本未知。建议作 feature flag 而非默认。

### v5-β：cranelift-only AOT（绕开 wasm IR）

**改动**：放弃 wasm 作中间表示。Relon IR 直接 lower 到 cranelift IR（`cranelift-frontend`），通过 `cranelift-jit` 生成 x86 / aarch64 native code。Linear memory bounds 检查手工 emit（只 emit 实际需要的），trap handler 用 Rust signal handling（libc `sigaction` + `sigsetjmp/siglongjmp`），capability gating 走 Rust 端的 capability bitmap match。

**ROI**：warm invoke **0.3-0.5 μs**（cranelift fn call 本身 ~100 ns，加最小 trampoline ~200 ns）。Cold start uncached 不变（cranelift 编译时间和 wasmtime cranelift 后端相同）。Cached cold start **~50 μs**（仅 mmap + relocate）。

**工程量**：XL（5-8 周）。codegen-wasm 完全重写到 codegen-cranelift；stdlib body 全部重新 lower（现有的 wasm IR Op stream 不复用）；trap unwind 全自己实现。

**安全（硬约束 sandbox 必备，按 v5 序言条款）**：
- Linear memory：用 Rust `Box<[u8]>` managed buffer，每次访问 emit cranelift `bounds_check` 指令（cranelift 原生支持 trap-on-OOB），等价 wasm memory.* 的 bounds 语义。
- Trap handler：Rust 端注册 SIGSEGV / SIGFPE handler，捕获 cranelift emit 的 bounds-trap / div-by-zero / unreachable，转 `Result<_, RuntimeError>` 返回。`sigsetjmp/siglongjmp` 跨 native code 边界回到 host。
- Capability gating：cranelift codegen 时把 host fn 调用走 indirect call 通过 capability bitmap 验过的 vtable，绕过等于 unreachable。
- Resource limit：cranelift 入口 prologue emit 一段 `cmp + cond_br trap` 对照 epoch deadline，比 wasmtime fuel 廉价。
- Spectre mitigation：默认开（cranelift 已支持 `enable_jump_tables=false` + bounds check 不 fold），高信任场景 feature flag 关，-100 ns/loop iter。

**ROI 后**：触达 LuaJIT trace JIT 同档（PUC Lua 是 sub-μs/op interpreter；LuaJIT trace JIT 是 sub-ns/op JIT trace）。具体落到哪档看是否做 trace recording —— Relon 当前是 AOT（编译一次跑多次），与 LuaJIT 的 hot loop tracing 模型不同，对 hot loop heavy 场景仍输 LuaJIT，但对 "cold start + 一次 invoke" 场景反胜。

### v5-γ：cranelift-object pre-AOT

**改动**：v5-β 的离线版。`relon-cli` 启动时 cranelift 输出 `.o` 目标文件，缓存到磁盘。运行时 `dlopen` + resolve symbol + `call $sym`。完全跳过 codegen 阶段。

**ROI**：cold start uncached **2-8 ms → 100 μs**（dlopen + reloc）；cached cold start **~10 μs**；warm invoke **0.3-0.5 μs**（同 β）。

**工程量**：v5-β 之上 +1 周（dlopen + symbol relocation 包装）。

**安全**：和 v5-β 同，但 cache 文件本身需要更严格 hash + signature 验证（dlopen 等于加载执行 unverified native code，比 wasm 危险得多）。

### v5-δ：**OUT OF SCOPE**（drops sandbox）

原版设想：检测高频 `#main(...)` 在 AOT 阶段输出 Rust 源码 + `cargo build` 集成到 host 二进制。`run_main` 走纯 Rust extern call。

**为何出局**：纯 Rust extern call 等于把脚本代码当 native code 直接执行，**违反 v5 序言的硬 sandbox 约束**（没有 bounds check、没有 trap 捕获、没有 capability 拦截）。要安全运行 untrusted Relon 脚本，这条路彻底不通。

如果未来出现 "host 完全信任 Relon source + sub-100 ns warm 是刚需" 的真实场景，单立 v6 系列，明确仅对 trusted source 启用 + 加 explicit opt-in flag + 文档警告。当前不规划。

### v5 路线累计估算

| 阶段叠加 | warm invoke | cached cold start | uncached cold |
| --- | ---: | ---: | ---: |
| v4 全做完 | 0.5-1 μs | 80 μs | 2-8 ms |
| +v5-α | **0.3-0.5 μs** | 80 μs | 2-8 ms |
| +v5-β / γ | **0.3-0.5 μs** | **~10 μs** (γ) | 100 μs (γ) |

到 v5-β/γ 已经物理跨过 LuaJIT trace JIT 的 cold start 优势线；warm invoke 仍是 LuaJIT 微胜（JIT trace 在 hot loop 里 sub-ns/op）。要追平 hot loop 需要自实现 trace JIT —— 在 v5 之后看必要性决定。

### 启动门槛

- v4 全做完是 v5 的前置（v4-e auto-tier 后才知道 wasm-AOT 哪些 workload 仍是瓶颈）
- v5-α / β / γ **不需全做**。看 v4 数据 + 用户场景：若 cold start 是痛点 → γ 优先；若 warm 是痛点 → β 优先；α 是低风险中等收益的过渡。
- 每个 v5 阶段也是 fresh agent + worktree isolation + 严格不砍 scope + sandbox 4 项硬约束（bounds check / trap / capability / resource limit）保留。

## 启动顺序 + 并行机会

各 phase 文件接触面分析（用于决定能否并行派 agent）：

| Phase | 主要 touch | 与谁冲突 |
| --- | --- | --- |
| v3++ b-7 SIMD ASCII | `crates/relon-codegen-wasm/src/lib.rs` + `crates/relon-ir/src/stdlib.rs`（upper/lower/title body） | v4-b, v4-c（同 stdlib.rs） |
| v4-d mmap cache | `crates/relon-codegen-wasm/src/cache.rs` 单文件 | 无 |
| v4-e auto-tier | `crates/relon/src/lib.rs` + `crates/relon-cli/src/main.rs` + `crates/relon-evaluator/src/lib.rs` | 无（独立 SDK 层） |
| v4-a dirty-leave | `crates/relon-codegen-wasm/src/evaluator.rs` (Pool-of-Stores) | v4-b 部分 overlap |
| v4-b arg specialization | `crates/relon-codegen-wasm/src/evaluator.rs` + `crates/relon-codegen-wasm/src/lib.rs` | v4-a, b-7, v4-c |
| v4-c stdlib helper inline | `crates/relon-codegen-wasm/src/lib.rs` + `crates/relon-ir/src/stdlib.rs` | b-7, v4-b |

**派 agent 顺序**（每次最多 2 个 in-flight）：

1. **当前**：v3++ b-6-tail (sequential，已 in_flight)。
2. **b-6-tail merge 后**：派 b-7 SIMD ASCII。**单 agent 跑**（stdlib bodies 高频改动，risk 大）。
3. **b-7 merge 后**：**并行**派 v4-d (mmap) + v4-e (auto-tier)。两 phase 文件无 overlap，独立 commit，两边都用 host-baseRef worktree。等待两份 task-notification 后顺序 merge（v4-d 先因为 cache.rs 较稳定）。
4. **v4-d + v4-e merge 后**：派 v4-a (dirty-leave) 单跑。Pool-of-Stores 改动跨多文件，慎并行。
5. **v4-a merge 后**：派 v4-b (arg specialization) 单跑。
6. **v4-b merge 后**：派 v4-c (helper inline) 单跑。
7. **v4-c merge 后**：v3++ + v4 完整收官，bench 全量更新 + 报告附录 A.26 总结。停在这里看用户决定是否开 v5。

**v5 阶段并行机会**：

v5-α (Wasmer headless) 是 runtime 替换，全局动；v5-β (cranelift-only) 是新 crate `relon-codegen-native`，与现有 codegen-wasm 完全独立但功能竞争；v5-γ 依赖 v5-β。

- v5-α + v5-β **可以并行**派两个 agent：α 在现 codegen-wasm 加 feature flag 切 Wasmer；β 在新 crate `relon-codegen-native` 起步。两边各自实现 4 项 sandbox 硬约束。等两份数据出来后用户决策 ship 哪条线（α 是 wasm 生态兼容路线，β 是 native code 极限路线）。
- v5-γ 必须等 v5-β 稳定后启动。

## Bench 落地条款

每个 v4 phase 完成：
- `cargo bench --bench wasm_aot_vs_tree_walk -- --quick` 跑全场景
- 更新 `docs/internal/wasm-bench-report-2026-05-16.md` 附录 A.21+（v17+ 起编号）
- merge commit subject 用 `merge(...): v4-X <feature>` pattern
- 全 workspace gate (`cargo test --features 'relon/wasm-aot' + clippy + fmt + wasm32 build`) 必须绿
