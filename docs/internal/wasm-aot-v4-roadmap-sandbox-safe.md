# Relon 性能路线图（v4-e → v5-β/γ → v6-γ，2026-05-18 终稿）

> **2026-05-18 第二轮规划修订**：用户决策规则 "与最终有益就留，纯中间过程临时产物就丢" + "目前项目没有包袱尚未发布"。
> 砍掉 9 个 phase（b-6-tail / b-7 / v4-a/b/c/d/f / v5-α / v6-α/β），保留 4 个 phase 直奔 LuaJIT trace tier。
> 路径估算 3-5 个月，相比原全顺序 6-9 个月省 3-4 个月。

## 直路 phase（按时序，2026-05-18 ultrathink 复核后）

| 序号 | Phase | 落点 | ETA |
| --- | --- | --- | ---: |
| 1 | v4-e auto-tier | IR op 数阈值切换 tree-walk / native-AOT；SDK 层 final-shape | 1 周 |
| 2a | v5-β-1 cranelift codegen infra + sandbox | 新 crate `relon-codegen-native`，cranelift-frontend / cranelift-jit 接入；4 项 sandbox 硬约束自实现（bounds check / trap handler / capability gating / resource limit）；HelloWorld `#main(Int)` 跑通；module cache 格式 + serialize / deserialize | 3-4 周 |
| 2b | v5-β-2 stdlib body re-lower + evaluator | 把 `crates/relon-ir/src/stdlib.rs` 所有 stdlib body builder 从 wasm IR Op stream 转成 cranelift IR；evaluator 集成（替代 wasm-AOT Pool-of-Stores）；wasm32 target 切换为 tree-walk-only；`relon-codegen-wasm` crate 从依赖图中拔除 | 3-4 周 |
| 3 | v5-γ cranelift-object cache | .o 文件 emit + dlopen + relocation；cached cold start 80→10 μs；cache 完整性 sha256（取代 wasmtime serialize 内部 hash） | 2 周 |
| 4 | v6-γ 完整 trace JIT | hot detection counter + trace recorder + trace IR optimizer + trace→cranelift IR + guard insertion + deopt machinery；differential testing 强制开启对照 cranelift-AOT baseline | 8-12 周 |

### Ultrathink 关键发现

1. **wasm32 target 问题**：`crates/relon-wasm` 是 browser playground 的 JS 绑定，需要 wasm 输出。v5-β cranelift-AOT 出 native code，不出 wasm。**决策**：wasm32 build target 使用 tree-walk-only，cranelift-AOT 仅 native target。Browser playground 性能不是关键路径，可接受。`relon-codegen-wasm` crate 在 v5-β-2 完成后**整个删除**，省双 backend 维护。

2. **v5-β 拆 β-1 + β-2**：原 6-8 周大块改动风险高。拆 codegen infra（HelloWorld 跑通）vs stdlib body 移植，每个 3-4 周可独立 ship + bench，β-1 完成后即可对照测试 deopt 接口（v6-γ 关键依赖）。

3. **v6-γ 最大风险是 deopt 正确性**。Trace 优化时假设某 type，guard 失败 deopt 必须正确还原 state 回 generic code。任何 bug 静默 corrupt 用户数据。**Mitigation**：v5-β-2 落地起立刻建 differential test harness：同一组 `#main` + args，分别跑 tree-walk / cranelift-AOT，输出必须 bit-identical。trace JIT 在此 baseline 上再增 deopt 路径全用例覆盖。

## 被砍掉的 phase（纯中间产物）

| 砍掉的 phase | 砍因 |
| --- | --- |
| v3++ b-7 SIMD ASCII（wasm v128） | v5-β cranelift 直出 native SIMD，更快且跨平台 |
| v4-a Pool-of-Stores dirty-leave | v5-β 无 wasmtime Store 概念，整套 pool 销毁 |
| v4-b 参数 bridge specialization (wasm 端) | v5-β cranelift codegen 同等机制，更直接 |
| v4-c stdlib helper 内联 | v5-β cranelift inliner 默认开 |
| v4-d mmap cache | v5-γ cranelift-object dlopen 是更原生的 cache 路径 |
| v4-f SIMD (wasm 端) | 同 b-7，cranelift native SIMD 接管 |
| v5-α Wasmer headless | 保留 wasm spec 的中间过渡，v5-β 直接跳过 wasm 整层 |
| v6-α PGO type specialization | v6-γ trace JIT in-runtime type recording 是 PGO 的超集 |
| v6-β PIC method dispatch | 同上，v6-γ 的 type specialization 自然处理单态 dispatch |

## 现状基线（main HEAD 截至 b-6-tail）

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

到 v5-β/γ 已经物理跨过 LuaJIT trace JIT 的 cold start 优势线 + 对齐 LuaJIT function call tier；hot loop sub-ns/op 仍要 v6 系列 trace JIT 追。

## v6 系列：trace JIT，追 LuaJIT hot loop sub-ns/op

**触发场景**：用户 2026-05-18 指出 "高频流 ETL 完全可能" —— 同一段 lambda 在 stream pipeline 上对 10⁵+ 条记录跑相同 transform。当前 wasm-AOT 单 invoke 7-12 μs，10⁵ 条记录 = 700 ms ~ 1.2 s，stream throughput bottleneck。LuaJIT trace JIT 同负载 < 10 ms。

**前置**：v5-β（cranelift-only AOT）完成。trace JIT 工作在 cranelift IR 之上，wasmtime / wasm 形式已退出。

### v6-α：PGO / type profile-guided specialization（最简方案）

**改动**：在 cranelift AOT 出第一份 generic code 之外，第一次 invoke 时记录类型 trace（每个 `Op` 入参的实际 `Value` tag）。第 N 次 invoke 后用 trace 触发 second compile pass，emit 专门按已观察类型分支的 code（generic 分支替换为直接 op；嵌入 guard 比较 tag，guard 失败 bail-out 回 generic）。Cache to disk。

**ROI**：tight loop（同一段算术 / 字段访问 chain 重复跑）warm invoke **-30 ~ -50%**。0.3 μs → 0.15-0.2 μs。对纯 ETL pipeline 累积 throughput **× 2-3**。

**工程量**：M（3-5 周）。Trace 记录只需要 instrument 入口 + 几个 hot ops（`LoadField*`, `Call`, `BitAnd` 等）；二轮 codegen 复用 v5-β 的 cranelift backend；guard 失败 deoptimization 走 cranelift trap → host Rust fallback。

**不是 trace JIT**：v6-α 是离线 specialization（profile → recompile，写入 AOT cache），不是 runtime recording。简洁、debuggable、且 cache 命中后零额外开销。

### v6-β：polymorphic inline cache (PIC) for method dispatch

**改动**：当前 `s.upper()` / `xs.length()` 等 method 调用通过 `stdlib_method_index(receiver_ty, name)` 静态查表 + indirect call。如果 receiver type 在运行时单态（绝大多数 Relon 程序如此），cranelift 出的 indirect call 还是要走 vtable load + jmp。PIC：在 call site 缓存最近 N 个 receiver type / function pair，hit 时 inline 直接 jmp 跳过 vtable 查表。

**ROI**：stdlib heavy 程序（如 string transform pipeline）warm invoke **-20-30%**。

**工程量**：S-M（2-3 周）。Cranelift 已有 inline-cache primitive (`call_indirect` 配合 `select`)；需要 host-side 维护 cache miss → backfill 逻辑。

### v6-γ：完整 trace JIT（LuaJIT-style）

**改动**：

1. **Hot detection**：在 cranelift 出的 fn entry + back-edge 处 emit increment counter；阈值（如 10）触发 trace mode。
2. **Trace recording**：执行模式从"跑 cranelift 出的 native"切到 "interpreted with trace recorder"。Recorder 接收每个 Relon IR op 的执行，linearize 成一条 trace（跨 branch 时只记录 taken 分支，未走的分支变 guard）。
3. **Trace optimization**：Trace IR 上做 constant folding / load forwarding / dead store elim / type specialization / loop invariant code motion。这部分对 Relon IR / cranelift IR 的设计要求最高。
4. **Trace compilation**：优化后的 trace → cranelift IR → native code。Trace 入口替换 hot function 的 dispatch table。
5. **Guard / bail-out**：每个 guard 失败时 deopt：跳回原 cranelift code 重新执行该 IR op 的 generic 分支。
6. **Trace cache + invalidation**：trace 缓存到内存，hot 程度统计；invalidate 时（如 type instability）evict。

**ROI**：hot loop（同 fn 反复跑相同 op chain）**sub-ns/op**。10⁵ 条记录 ETL：1.2 s → ~10 ms。

**工程量**：XL（8-12 周）。LuaJIT 用 ~10000 行 C 实现 trace JIT；我们走 Rust + cranelift，估计 5000-8000 行 + 大量调试。最大风险是 guard 失败 deopt 路径正确性 —— 任何 bug 会导致 trace 出错误结果或 panic。

**Sandbox**：trace 模式跑的也是 cranelift 出的 native code，沿用 v5-β 的 4 项 sandbox 实现。guard 失败 bail-out 是 controlled deopt，不破坏 trap handler / capability gating。

### v6 路线累计估算

| 阶段叠加 | warm invoke (single) | hot loop op (10⁵ iters) |
| --- | ---: | ---: |
| v5-β / γ 全做完 | 0.3-0.5 μs | ~30-100 ns/op |
| +v6-α (PGO) | 0.15-0.3 μs | ~10-30 ns/op |
| +v6-β (PIC) | 同 α | ~5-15 ns/op |
| **+v6-γ (trace JIT)** | 同 | **~1-5 ns/op** （**LuaJIT trace tier**） |

stream ETL 1M 记录的端到端 throughput 估算（同一 transform 重复）：

| 阶段 | 单条 cost | 1M throughput |
| --- | ---: | ---: |
| 当前 (v15 b-5) | 7 μs | 143 K rps |
| v5-β | 0.3 μs | 3.3 M rps |
| +v6-α | 0.15 μs | 6.7 M rps |
| +v6-γ | < 0.01 μs (hot trace) | **> 100 M rps** |

100 M rps 量级在 LuaJIT 同负载实测里也是物理上限，对应 single-core ETL pipeline 用满。

### v6 启动顺序

1. **v6-α (PGO) 先做**：单点 ROI 高 + 工程量小 + 不动 sandbox 模型。3-5 周。
2. **v6-β (PIC) 随后**：在 α 数据反馈之后决定 —— stdlib heavy workload 占比高就做，否则跳过。
3. **v6-γ (trace JIT) 最后**：α + β 之后如 hot loop benchmark 仍在 30 ns/op 以上、且 stream ETL 是高优先 use case，启动。否则暂停在 v6-β 也已经对齐 LuaJIT 大部分场景。

### 并行机会

v6-α 和 v6-β 改动文件无 overlap（α 在 profile + AOT cache + 二轮 codegen；β 在 cranelift inline cache infra）。可以并行派两个 agent。v6-γ 必须等 α/β 稳定后串行启动（trace recorder 需要看到 α 的 type 信息 + β 的 PIC 数据来决定哪些 op 值得 trace）。

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
