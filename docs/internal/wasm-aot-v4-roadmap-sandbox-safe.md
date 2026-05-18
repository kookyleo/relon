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

两条都**动 sandbox**，明确不在 v4 范围。如未来要追，单立 v5 系列。

## 启动顺序建议

1. **v4-e**（auto-tier）+ **v4-d**（mmap）—— 两个独立的快胜利，一周内能 ship。
2. **v4-a**（dirty-leave）—— 单点 ROI 最高，但工程量最大。优先于 b/c。
3. **v4-b**（参数 specialization）→ **v4-c**（helper inline）—— b 拿 1.5 μs，c 再拿 1 μs，叠加后看 criterion 数字定 v4-f 优先级。
4. **v4-f**（SIMD）——v3++ b-7 落地后的延续，看 ASCII workload 比重决定值不值得做完整版还是只做 16-byte chunk。

## Bench 落地条款

每个 v4 phase 完成：
- `cargo bench --bench wasm_aot_vs_tree_walk -- --quick` 跑全场景
- 更新 `docs/internal/wasm-bench-report-2026-05-16.md` 附录 A.21+（v17+ 起编号）
- merge commit subject 用 `merge(...): v4-X <feature>` pattern
- 全 workspace gate (`cargo test --features 'relon/wasm-aot' + clippy + fmt + wasm32 build`) 必须绿
