# Relon 性能报告 — v5-β-2 stage 4（2026-05-18）

> 本文档定位：v5-β-2 stage 4 收官的**性能交付物**。继承
> `docs/internal/wasm-bench-report-2026-05-16.md`（已 [archived]）
> 的"性能报告"角色，但记录对象换成 cranelift-AOT vs tree-walk —
> wasm-AOT 后端已在 stage 4 退役（commit `b6b4470`）。
>
> Bench 入口：`cargo bench -p relon-bench --bench cranelift_aot_vs_tree_walk`。
> 源码：`crates/relon-bench/benches/cranelift_aot_vs_tree_walk.rs`。

## 一、执行摘要

针对纯算术 `#main(Int x, Int y) -> Int : x + y` 场景，criterion 0.5
在 release profile（fat LTO + codegen-units = 1）下采集 50-sample ×
5s 测量窗口的数据（stage 4 现场实测，2026-05-18）：

| 探针 | 中位数 | low (95 % CI) | high (95 % CI) | 含义 |
|---|---:|---:|---:|---|
| `cranelift_cold` | **275.4 μs** | 275.3 μs | 275.6 μs | 合成 IR + cranelift JIT compile + finalize |
| `cranelift_warm` | **415.2 ns** | 413.1 ns | 419.9 ns | 复用 evaluator，单次 `run_main(args)` 全开销 |
| `tree_walk_total` | **1.260 ms** | 1.250 ms | 1.272 ms | 每次 iter 重建 Context + TreeWalkEvaluator |
| `tree_walk_warm` | **2.352 μs** | 2.348 μs | 2.361 μs | 复用 walker，单次 `run_main` |

数字与 stage 1 同套件（`v5b1_arithmetic`，archived，cranelift cold
245 μs / warm 390 ns）量级一致 —— buffer-protocol entry shape 在
stage 2 落地之后单 op 路径上多了一对 4-arg marshaling，导致 warm
路径多 ~25 ns、cold 路径多 ~30 μs。这是符合预期的常数级开销，并未
改变 cold/warm tier 的定性结论。

三条结论性观察：

1. **cranelift warm invoke 415 ns**：已落进 LuaJIT trace tier 量级
   （目标 0.3-0.5 μs）。整条路径只有 `Arc::as_ptr` → `catch_unwind`
   wrapper → 直接 `extern "C"` 调用 + 4-arg buffer-protocol marshal。
2. **cranelift cold 275 μs**：跳过 parse / analyze / lower 三关，
   单纯 JIT compile + finalize。比 wasm-AOT stage 1 的 4.20 ms cold
   start 快约 **15×**（wasm 路径要付双层 cranelift：wasm encode +
   wasmtime 内部 cranelift compile）。
3. **tree-walker warm 2.35 μs**：cranelift warm 比 tree-walker warm
   快 **5.7×**，差距来自 IR dispatch loop overhead 与 schema look-up
   chain；cranelift 的 native code 把这两块全部固化到机器码里。
   cranelift cold（275 μs）摊销到一次 run_main 也只需 ~660 次 warm
   调用就能赢过 tree-walker warm —— 典型 long-running 服务进程
   一秒内就能达成。

## 二、Stage 4 现场实测

> 实测命令：`cargo bench -p relon-bench --bench cranelift_aot_vs_tree_walk`。
> 测试机：dev workstation（同 stage 3 报告）。release profile：fat
> LTO + codegen-units = 1，criterion measurement_time = 5s, sample = 50。

stage 4 现场实测（2026-05-18）：

```
v5b2_stage4_arithmetic/cranelift/cold   [275.29 µs 275.44 µs 275.62 µs]   (3/50 outliers, all high mild)
v5b2_stage4_arithmetic/cranelift/warm   [413.14 ns 415.21 ns 419.93 ns]   (6/50 outliers)
v5b2_stage4_arithmetic/tree_walk/total  [1.2503 ms 1.2599 ms 1.2722 ms]   (12/50 outliers high severe)
v5b2_stage4_arithmetic/tree_walk/warm   [2.3477 µs 2.3519 µs 2.3606 µs]   (3/50 outliers)
```

`tree_walk/total` 的 12 个 high-severe outlier 反映 tree-walker 冷启动
路径包含 `relon_analyzer::analyze` + `Context` 装配 + `prepare_in_place`
（stdlib / decorator 注入），上面随 GC pause / 调度抖动较大；中位数
仍然稳定在 1.26 ms。

archived 对照基线（stage 1 cranelift β-1，2026-05-18 早些时同机
跑出来的 `v5b1_arithmetic`）：

```
v5b1_arithmetic/cranelift/cold   245.5 µs        (stage 4 多 ~30 µs，来自 buffer-protocol prologue + tail-cursor init)
v5b1_arithmetic/cranelift/warm   390.4 ns        (stage 4 多 ~25 ns，4-arg marshal)
v5b1_arithmetic/wasm/cold        4.20 ms   [archived]
v5b1_arithmetic/wasm/warm        1.09 µs   [archived]
```

## 三、Cranelift backend 当前覆盖范围（v5-β-2 stage 3 + stage 4）

stage 3 报告（`docs/internal/v5-beta-2-stage3-report-2026-05-18.md`）
记录的 51/52 corpus 覆盖度在 stage 4 保持不变 —— stage 4 只做了
"删除 wasm-AOT" + bench 改造，不改 IR lowering 矩阵。

| 维度 | 覆盖 | 备注 |
|---|---:|---|
| `arith_control` | 27/28 | 唯一缺口 `let_chain` 是 analyzer-rejected，tree-walk-only by construction |
| `stdlib_simple` | 9/9 | length / substring / starts_with / abs / min / max / list_int_sum / list_int_max / list_string_sum |
| `stdlib_memory` | 4/4 | concat / substring / starts_with（scratch arena） |
| `stdlib_case_fold` | 5/5 | upper / lower / title + Greek / Turkish locale 变体 |
| `stdlib_list` | 2/2 | list_int_sum / list_int_max（pure iteration shapes） |
| `stdlib_normalize` | 2/2 | nfc / nfd / nfkc / nfkd |
| `dict_return` | 2/2 | 含 sub-record + tail-cursor 协议 |
| **总计** | **51/52** | **唯一缺口为 analyzer-only case，非 cranelift 缺陷** |

## 四、与 wasm-AOT 历史对照（[archived]）

stage 1 的 wasm-AOT 性能档案完整保留在
`docs/internal/wasm-bench-report-2026-05-16.md`，附录 A.5 ~ A.21 标记
为 `[archived]`。核心对照数据（stage 1 `v5b1_arithmetic` 套件，
2026-05-18 同机实测）：

| 后端 | cold | warm |
|---|---:|---:|
| cranelift-AOT (stage 4) | 275 μs | 415 ns |
| cranelift-AOT (stage 1) | 245 μs | 390 ns |
| wasm-AOT [archived] | 4.20 ms | 1.09 μs |
| 倍率（cranelift stage 4 优 = ↑）| 15× | 2.6× |

wasm-AOT 在 cold start 上劣势主要来自双层 cranelift（自身 wasm
encode + wasmtime 内部 cranelift compile）；warm 上劣势主要来自
wasmtime `Store::new + Linker::instantiate` + buffer marshal 的
固定开销。这两块都不是 codegen-wasm 实现质量问题，而是 wasmtime
本身的成本结构 —— 这也是 v5-β-2 stage 4 选择直接退役 wasm-AOT 的
直接原因（cranelift 既快又覆盖更广）。

## 五、v5-γ / v6-γ 入口

stage 4 已完成的：

- ✅ wasm-AOT 后端 + crate + facade + CLI 全删干净（commit
  `b6b4470 chore(workspace): retire relon-codegen-wasm crate +
  wasm-AOT facade`）。
- ✅ bench 改造为 `cranelift_aot_vs_tree_walk`。
- ✅ wasm-bench-report-2026-05-16 标记 deprecation prologue + 附录
  A.5 ~ A.21 标记 `[archived]`。
- ✅ 本报告（relon-perf-report-2026-05）替代 wasm-bench-report 的
  性能交付物角色。

stage 4 保留的 deferred 项（来自 stage 3 报告，stage 4 维持原
deferred 状态，下放给 v5-γ 跟进）：

1. **`Op::CallNative` 完整 indirect dispatch（Phase C.1）** — 当前
   `CheckCap` 已验证 vtable 槽非空；下一步 `call_indirect` + per-
   `(param_tys, ret_ty)` cranelift `SigRef` 表 + 指针 indirect arg
   marshaling。
2. **`Op::CallClosure` + 闭包入参 list 高阶（Phase C.4）** — 闭包
   ABI（`closure_table` on `IrModule` 已 plumb 到 codegen 入口）+
   captures buffer 实例化 + `call_indirect` against captured fn ptr。
3. **`Op::Loop { result_ty: Some(_) }` + `Op::BrTable` + 内层 loop
   `RESOURCE_CHECK_INTERVAL` 重查节奏（Phase C.2）** — 当前 IR
   bodies 全部 `result_ty: None` + 显式 acc 累加变量；带 yield 的
   loop 走 v5-γ。
4. **真 `sigsetjmp` / `siglongjmp` trap handler（Phase C.3）** —
   `signal-hook` 0.3 + libc，进程级 install once。当前
   `catch_unwind` 路径在功能上等价；2 ns/guard 收益不是热路径关键。
5. **`cranelift-object` 模块缓存** — 把 .o 序列化到磁盘，cold
   start 可跳过整个 JIT compile（仅做 mmap + relocation）。设计稿
   见 `docs/internal/v5-gamma-cranelift-object-cache-design.md`。

v6-γ 远期：trace JIT 与本路径正交。设计稿
`docs/internal/v6-gamma-trace-jit-design.md`。

## 六、Bench 数据采集方式

```bash
cd /path/to/relon
cargo bench -p relon-bench --bench cranelift_aot_vs_tree_walk
# 默认 measurement_time=5s × sample_size=50
# 输出落在 target/criterion/v5b2_stage4_arithmetic/*/estimates.json
```

CI 上 bench 跑的是同一份 bench binary（见
`.github/workflows/bench.yml`）；CI 数字噪声较大（共享 runner，
无 CPU pin），所以以本地 perf-runner 数字为准。

## 七 bis、Stage 5 Phase C 落地（2026-05-18 同日续）

stage 5 收尾把 stage 4 报告里列为 v5-γ 跟进的 Phase C 4 项全部落地：

| Phase | Scope | Stage 5 commit | 状态 |
|---|---|---|---|
| C.1 | `Op::CallNative` full indirect dispatch via cap vtable | `c55e762` | ✅ |
| C.4 | `Op::MakeClosure` + `Op::CallClosure` closure ABI | `7d3d298` | ✅ |
| C.2 | `Op::Loop result_ty != None` + `Op::BrTable` + back-edge cadence | `27b6f85` | ✅ |
| C.3 | signal-hook trap handler infrastructure（替代 catch_unwind 路径的基础设施） | `5718c6e` | ⚠ infrastructure-only：完整 sigsetjmp 长跳留 v6-γ |

### Phase C.1 — CallNative dispatch

`Op::CallNative` 从 stage 3 / stage 4 的"CheckCap 验存在"升级成完整
`call_indirect`：

* 每个 call site 先 `cap_lookup(state, cap_bit)` 取 host fn ptr。
* 空指针走 `TrapKind::CapabilityDenied`（即便 sandbox `capability_check`
  关掉也要 null-check —— 空 indirect call 会 segfault）。
* 用 IR-declared `(param_tys, ret_ty)` 现场构造 cranelift `Signature`
  → `import_signature` → `call_indirect`。
* `cap_bit == NO_CAPABILITY_BIT` 时 fallback 用 `import_idx` 作 vtable
  lookup key（兼容 host SDK 把无 cap 入口按 import_idx 注册的惯例）。

测试覆盖：nullary fn / 1-arg / 2-arg / capability denied trap /
side-effect mutation / signature mismatch refuse（6 case）。

### Phase C.4 — Closures

每个 lambda 都被编译为独立的 cranelift function，签名
`(state, captures_ptr: i32, params...) -> ret`：

* `IrModule::closure_table[slot] -> funcs[idx]` 给出 lambda 的源
  位置。`compile_module_with` 在编译 entry 之前就 `declare_function`
  所有 lambda，编译 entry 之后再逐个 `define_function`。
* `finalize_definitions` 之后 host 把每个 lambda 的 fn ptr resolve
  成 `usize`，存进 `Box<[usize]>`。
* `SandboxState::closure_table_base`（新 field，offset 40）由 host
  trampoline 装上 `Box` 的首址。
* `Op::MakeClosure` 在 scratch 区分配 8-byte handle +
  captures struct，写 `[fn_table_idx][captures_ptr]`，捕获值按
  `ClosureCapture::offset` 写入；push handle 的 arena offset。
* `Op::CallClosure` 加载 handle 的两个字段，按
  `closure_table_base[fn_table_idx]` 拿 fn ptr，null-check 防御，
  emit `call_indirect` with `(state, captures_ptr, args...)` 签名。

测试覆盖：无捕获 lambda / 带 1 个 I64 capture / 2-arg lambda / 多
lambda 在同模块各占独立 slot（5 case）。

### Phase C.2 — Loop result_ty + BrTable + 节奏

* `Op::Loop` / `Op::Block` 现在通过 cranelift block-parameter 实现
  `result_ty != None` 的 yield-value 模式。loop header 的 block-param
  即是 loop-carried accumulator，每条 back-edge 通过 jump args 重新
  喂入；fall-through 落到 cont block。
* `Op::BrTable` 用 cranelift `br_table` + `JumpTableData` 实现，
  per-arm yield-args 统一前置。
* `RESOURCE_CHECK_INTERVAL = 1024` cadence：每个 loop 在 header
  block 上声明一个 I64 计数变量，`emit_br` 在 is_loop 的 back-edge
  位置 emit `++counter; if (counter & 1023) == 0 emit_resource_check`。

测试覆盖：yielding loop sum 1..=n / br_table default / br_table case
0 / br_table case 1 / 100k iter 不 trap / 0-ns deadline trap（6 case）。

### Phase C.3 — signal-hook handler infrastructure

stage 5 落地了 `crate::trap_handler` 模块 + `signal-hook` 0.3 +
`signal-hook-registry` 1.4 依赖：

* `install_global_signal_handler()` 用 `OnceLock` 装一次性，handler
  内只触 thread-local + atomic，符合 async-signal-safe。
* 通过 `register_signal_unchecked` 注册 SIGSEGV / SIGFPE / SIGILL —
  signal-hook 默认把这三个标记为 forbidden 是为防止库代码意外抢占
  Rust panic runtime；我们的 handler 不分配、不锁、不跑 Drop，绕过
  forbidden 检查是合理的。
* `invoke_legacy_entry` / `invoke_buffer_entry_with_scratch` 每次
  调用前 install + reset，调用后 `dispatch_post` 先看 signal slot
  再看 sandbox trap_code（signal 来自 hardware/OS layer，优先级高
  于 codegen 主动 emit 的 trap）。

**这是 infrastructure-only 落地**：完整的 sigsetjmp / siglongjmp
长跳被推到 v6-γ trace JIT，原因是

* libc crate 不 expose `sigsetjmp`（glibc 上是 macro，跨平台差异大）。
* 现有 `catch_unwind` shield 在 cond_trap-emitted trap 的功能上等价；
  signal-hook 给的是 hardware-side memory-safety bug 的兜底。
* 性能差距是 micro（cold path），不是热路径关键。

测试覆盖：handler 幂等装载 / reset+read / 4 个 signal-to-trap-kind
映射（6 case）。

### Stage 5 实测 bench 数据（2026-05-18 同日）

```
v5b2_stage4_arithmetic/cranelift/cold    [289.66 µs 293.37 µs 298.41 µs]   (2/50 outliers, all high mild)
v5b2_stage4_arithmetic/cranelift/warm    [398.07 ns 400.49 ns 404.66 ns]   (8/50 outliers)
v5b2_stage4_arithmetic/tree_walk/total   [1.2727 ms 1.2890 ms 1.3054 ms]   (1/50 outliers high mild)
v5b2_stage4_arithmetic/tree_walk/warm    [2.3526 µs 2.3654 µs 2.3893 µs]   (7/50 outliers)
```

vs stage 4 同套件（同机，2026-05-18 早晨）：

| 探针 | stage 4 中位数 | stage 5 中位数 | Δ |
|---|---:|---:|---:|
| cranelift cold | 275.4 µs | **293.4 µs** | +18 µs（+6.5 %），来自 codegen 新增 4 个 Op 分支 + closure_func_ids 预 declare + signal handler install once |
| cranelift warm | 415.2 ns | **400.5 ns** | **−15 ns（−3.5 %）**，意外的小幅改进，主要是 cranelift opt_level=speed 把 emit_block 的 fall-through 跳转去重 |
| tree_walk total | 1.260 ms | 1.289 ms | +29 µs（+2.3 %），噪声范围 |
| tree_walk warm | 2.352 µs | 2.365 µs | +13 ns（+0.6 %），噪声 |

**cranelift warm 400 ns 与 LuaJIT trace tier 0.3-0.5 µs 目标完全一致，
仍领先 tree-walk warm 5.9 ×**。stage 5 phase C 的开销集中在 cold
path（多走 4 个 Op 的 lowering 分支），warm path 反而因 cranelift
optimizer 更好地利用 block fall-through 的 SSA structure 而小幅
变快。

## 七、Gate（stage 4 final）

stage 4 收尾 gate（feature 调整后，`cranelift-aot` 是 `relon`
crate 的 default feature，CLI / 测试不再需要显式 feature flag）：

| Gate | 命令 | 结果 |
|---|---|---|
| build | `cargo build --workspace` | ✓ |
| test | `cargo test --workspace --no-fail-fast` | 1483 passed / 0 failed |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| fmt | `cargo fmt --all -- --check` | ✓ |
| wasm32 | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ |

baseline 1790 → stage 4 1483 是因为 wasm-AOT 移除带走了 ~300
个 wasm-codegen specific tests（aot_cache_smoke、binary_handshake_smoke
等 18 个 smoke files）。stage 4 没有引入新测试 —— Phase C 系列
deferred 项推到 v5-γ，所以 test 数量目前是 1483。

---

**Author**: Relon perf 直路 v5-β-2 implementer agent（stage 4）
**Date**: 2026-05-18
**License**: Apache-2

## 八、v5-γ — cranelift-object cache infrastructure（2026-05-18）

v5-γ phase 在 codegen-native 之上接入了 `relon-object-cache` 与
`relon-object-link` 两个 prep-agent 提供的 crate，为 cold-start 路径
铺设了 cranelift-object emit + ld -shared 链接 + on-disk 持久化 +
HMAC 完整性校验 的完整管线。

### 8.1 新增 API 表面

| 入口 | 行为 |
|---|---|
| `CraneliftAotEvaluator::from_source_with_cache(src, cache_dir)` | 走完整 from_source 管线，并把 ET_DYN bytes 与 IR bincode 写到 cache_dir（best-effort，所有失败 downgrade 为 tracing::warn） |
| `CraneliftAotEvaluator::from_cache_dir(src, cache_dir)` | 校验 cache pair 的 HMAC + integrity + metadata；命中后返回 `Some(evaluator)`，否则 `Ok(None)`。**当前实现内部仍走 from_source；真正的 dlopen-exec 短路需要 codegen.rs 把 `relon_now` / `relon_raise_trap` / `relon_cap_lookup` 改走 `__relon_capability_vtable` 间接调用（设计稿 §2.3，留作后续 phase）** |
| `compute_source_hash(src, sandbox)` | sha256 over canonical (source‖sandbox-bits‖triple‖generator-version) — 用作 cache 文件名 stem |
| `default_cache_dir()` | `$XDG_CACHE_HOME/relon` → `$HOME/.cache/relon` → `temp_dir/relon-cache` |

`AutoEvaluator::build_aot` 接入了上述两个 entry point：先 try
`from_cache_dir`，miss 再走 `from_source_with_cache`。Cache 写失败
不影响 live invocation。

### 8.2 实测数字（criterion `--quick`，host: linux x86_64）

| 场景 | v5-β-2 stage 4 | v5-γ | 变化 |
|---|---|---|---|
| `cranelift_cold`（synthetic IR + JIT） | ~273 µs | ~275 µs | 持平（无回归） |
| `cranelift_warm`（preassembled） | ~400 ns | ~391 ns | 持平 |
| `v5_gamma_cached_cold_start/cranelift_cached/cold` | — | ~2.68 ms | 新增 |
| `tree_walk_warm` | ~2.4 µs | ~2.37 µs | 持平 |

### 8.3 与 15 µs 目标的差距

设计稿目标 `cached cold start ≤ 15 µs`。本次落地 ~2.68 ms（差
~180×），原因：

1. **dlopen-exec 路径未激活**。当前 `from_cache_dir` 命中后仍走
   parse + analyze + lower + JIT，因为 cranelift-object emit 出来
   的 ET_DYN 还引用了 `relon_now` 等 sandbox 辅助符号。要 dlopen
   就 resolve 它们，要么主程序 `-rdynamic`（脆弱），要么改 codegen
   走 vtable 间接（多日 refactor）。
2. **IR-cache 快路径覆盖不足**。`crate::cache::serialize` 是 v5-β-1
   遗留 narrow 实现，只覆盖 legacy `(I64...) -> I64` envelope；
   现在 buffer-protocol IR 走它会丢 ops 触发 stack underflow，所以
   `from_cache_dir` 没法用 IR-cache 跳过 parse + analyze + lower。

### 8.4 已落地基础设施 & 留作 follow-up 的工作

**落地**：

- ET_REL emit：`object_cache_integration::emit_entry_stub_object`
  用 cranelift-object ObjectModule emit `relon_main_entry` +
  `__relon_capability_vtable` reservation。tests/object_cache_
  integration.rs 中的 `loader_round_trip_from_emitted_stub_bytes`
  端到端验证 emit → ld -shared → memfd_create → dlopen → dlsym →
  call 全链路在 linux-x86_64 上工作。
- HMAC + integrity：cache 文件用 per-installation HMAC-SHA256
  key（`$XDG_DATA_HOME/relon/cache-key`，mode 0600）防第三方投放。
  tamper detection test 通过：篡改对象字节 → 下次 from_cache_dir
  返 None 并删除 corrupt 文件。
- 并发安全：4-thread `from_cache_dir` race 测试通过。
- 跨平台降级：non-linux / non-x86_64 host 自动跳过 cache，走纯
  JIT，logged at info level。
- 错误降级：ld 缺失 / 失败 / HMAC key 不可用 / 文件损坏 — 每条都
  归属 tracing::warn / error / info，without 影响 live invocation。

**留 follow-up**：

- **dlopen-exec 短路**（最大头，预期 ~10-15 µs cached cold start）：
  改 codegen.rs 让 `relon_now` / `relon_raise_trap` /
  `relon_cap_lookup` 与 closure-table base 全走
  `__relon_capability_vtable` GlobalValue 间接调用；host 在
  dlsym 拿到 vtable 地址后 populate 各 slot。
- **IR-cache 全 op 覆盖**：把 `crate::cache::serialize` 的 narrow
  serde 换成 `relon_ir::ir::Module` 整体的 serde derive，让
  `from_cache_dir` 能 skip parse + analyze + lower 即便不走
  dlopen-exec 也能拿到 ~80 µs cached cold start。
- **LuaJIT 对照**：等 dlopen-exec 落地后再跑端到端对比。

### 8.5 v5-γ Gate

| Gate | 命令 | 结果 |
|---|---|---|
| build | `cargo build --workspace` | ✓ |
| test | `cargo test --workspace` | 1607 passed / 0 failed |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| fmt | `cargo fmt --all -- --check` | ✓ |
| wasm32 | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ |

1591 → 1607 = +16 tests，覆盖 cache write / load / corruption /
metadata mismatch / concurrent hit / loader smoke 等场景。

---

## 九、v5-γ stage 2 — vtable indirection + dlopen-exec（2026-05-19）

stage 1 留的最大 follow-up（dlopen-exec 短路）此阶段落地。

### 9.1 改动概览

1. **`crates/relon-codegen-native/src/vtable.rs`** 新增模块定义
   `VtableSlot` enum（RelonNow / RelonRaiseTrap / RelonCapLookup，
   总 3 slot，reserve 32 slot 占位），`__relon_capability_vtable`
   symbol 名，以及 `populate_vtable` 把 host fn 指针写进 slot 的
   `unsafe fn`。
2. **`codegen.rs` 改造**：所有 host helper 调用从 `Linkage::Import` +
   `call FuncRef` 改成 `__relon_capability_vtable` data symbol +
   `load(vtable_base + slot_offset)` + `call_indirect(sig, fn_ptr,
   args)`。entry / lambda / trap_block tail 三个调用站点共享同一
   `emit_indirect_host_call` helper；`Codegen` 结构体加
   `emit_host_fn_call(slot, args)` 方法。
3. **拆出 `lower_module_into<M: CrModule>`** 让 JIT 路径（`JITModule`）
   与 cranelift-object 路径（`ObjectModule`）共用同一份 lowering。
4. **`compile_module_to_object_bytes`**：parallels JIT 路径，产出
   ET_REL bytes 给 `relon-object-link::link_to_dyn`。dlopen 后 ET_DYN
   只引用 `__relon_capability_vtable` 一个外部 symbol。
5. **`schema_cache.rs` 新模块**：side-loaded `.relon-schema-v1` 文件
   持 (main_schema, return_schema, param_names, const_data,
   closure_count, entry_shape, entry_arity, entry_range)；
   `from_cache_dir` 用它跳过 parse + analyze + lower。
6. **`from_cache_dir` 改造**：命中后走 `LoadedObject::from_bytes`
   memfd + dlopen + dlsym(run_main / vtable / __closure_N) →
   `populate_vtable` → 构造 `CraneliftAotEvaluator { _module:
   EntryBacking::Dlopen(loaded), ... }`。完全 skip JIT。
7. **GENERATOR_VERSION** 升到 `"relon-codegen-native v5-gamma 2"`，
   stage 1 cache 文件自失效。

### 9.2 实测数字（criterion `--quick`，host: linux x86_64）

| 场景 | stage 1 (2026-05-18) | stage 2 (2026-05-19) | 变化 |
|---|---|---|---|
| `cranelift_cold`（synthetic IR + JIT） | ~275 µs | ~278 µs | 持平（vtable 间接调用对 cold 无影响） |
| `cranelift_warm`（preassembled） | ~391 ns | ~398 ns | 持平（warm 路径多一次 vtable load，~7ns） |
| `v5_gamma_cached_cold_start/cold` | ~2.68 ms | **~339 µs** | **−7.9×** |
| `v5_gamma_cached_cold_start_full/cold_full` | — | **~350 µs** | 新增（cached cold start + 1 invoke） |
| `tree_walk_warm` | ~2.37 µs | ~2.38 µs | 持平 |

### 9.3 与 15 µs 目标的差距 — 分阶段 latency 表

`tests/vtable_latency_breakdown.rs`（release profile）打出的细分：

| 阶段 | 时长 | 占比 | 备注 |
|---|---|---|---|
| `cache_load` | ~259 µs | 52% | 读 ELF bytes from disk + HMAC verify。冷文件系统 + sha256(每文件) 是大头 |
| `dlopen+dlsym` | ~179 µs | 36% | `memfd_create` + write + `/proc/self/fd/N` dlopen + ld.so 重定位 + 3 次 dlsym |
| `schema_decode` | ~52 µs | 10% | `serde_json::from_slice` ~500 字节 |
| `vtable_populate` | ~6 µs | 1% | 3 次 8-byte 写 |
| **total** | ~496 µs | 100% | warm FS cache 下 criterion 取得 ~340 µs |

stage 2 把 cached cold start 从 2.68 ms 降到 ~340 µs，但仍然
比 15 µs 目标高 ~22×。要继续推到 ≤ 15 µs 需要 stage 3 级别的
架构改动：

- **mmap + RELRO**：把 cache file mmap 进内存而非每次 read，
  避免冷 fs 读 + HMAC 重算（如果 host trust on cache_dir
  ownership）。预期省 ~200 µs。
- **avoid dlopen**：自己写一个最小 ELF loader 走 mmap +
  in-process relocation（cranelift 已知它发的是 PIC RIP-relative
  + GOT/PLT-less code，loader 不需要做太多 work），跳过 ld.so 的
  通用-purpose codepath。预期省 ~120 µs。
- **schema cache 二进制化**：换掉 serde_json 用 bincode + 手写
  custom format（绕开 `TypeRepr` 的 `tag = "kind"` 限制）。预期
  省 ~40 µs。

合起来理论上能进到 < 30 µs，但 stage 3 不是这一轮的范畴；本
stage 的核心交付是 vtable indirection + dlopen-exec 真的能跑
（5 个新 vtable_indirection 测试 + 10 个原 object_cache_integration
测试全过）。

### 9.4 关键决策

1. **vtable 间接，不 -rdynamic**。stage 1 报告里列了 `-rdynamic`
   作为备选；本 stage 拒绝它，因为 `cargo test` 默认不带，embedded
   host 也不可能控制 build flag。
2. **schema cache 用 serde_json，不 bincode**。`TypeRepr` 是
   `#[serde(tag = "kind")]` 内部 tag，bincode 1.x 不支持
   `deserialize_any`。换 bincode 要么改 TypeRepr 的 serde（污染
   `relon-eval-api`），要么手写 encoder。serde_json 在 ~50 µs 这
   一段不是 critical。
3. **closure-table dispatch 保留间接基址**。`Op::CallClosure` 已
   经走 `state.closure_table_base[idx]` 间接，与 vtable 间接是相同
   pattern；不需要让它经过 vtable，dlopen 后 host 用 dlsym 重新
   resolve 每个 `__closure_N` 即可。

### 9.5 v5-γ stage 2 Gate

| Gate | 命令 | 结果 |
|---|---|---|
| build | `cargo build --workspace` | ✓ |
| test | `cargo test --workspace` | 1632 passed / 0 failed |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| fmt | `cargo fmt --all -- --check` | ✓ |
| wasm32 | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ |

1607 (stage 1) → 1632 (stage 2) = +25 tests。新增覆盖：
- `vtable.rs` 4 个 unit tests（slot offset / count / reserved
  headroom / populate_vtable smoke）。
- `tests/vtable_indirection.rs` 5 个集成测试（cached cold start
  through dlopen-exec for + / - / div-by-zero 三种 entry body）。
- `tests/vtable_latency_breakdown.rs` 1 个 prof probe（不算
  Gate 数字，但保留在 tree 里方便回归）。
- `schema_cache.rs` 3 个 unit tests（round-trip / magic / digest）。
- 已有 `object_cache_integration.rs` 10 个测试全继续通过（schema
  cache 副 file 加进流程后没动它们）。

---

## 十、v6-γ M4 (trace JIT recording + deopt machinery)

### 10.1 范围

M4 把 v6-γ trace JIT pipeline 从 "可以手工 build buffer → install" 推进
到 "IR walker 边走边录 → 自动 install → guard 失败回 generic"。具体落地
（详见 `docs/internal/v6-gamma-m4-stage-report-2026-05-19.md`）：

1. `record_guard` 同步到 buffer 侧表（修 `EmitError::OrphanGuardOp`）。
2. `TraceRecordingEvaluator` IR walker 覆盖 Phase-1 hot subset（const +
   arith + cmp + locals + return on I64）。
3. `__relon_jump_to_recorder` 真正实现：thread-local
   `RecordingRegistration` 注册 → walker 边录边跑 → optimizer →
   emitter → JIT install。
4. `HostHookTable::save_deopt` 通过 `default_host_hooks()` wire（emitter
   仍用 extern symbol，table 平行保留供 host 查 deopt 状态）。
5. `TraceJitState::invoke_with_fallback` deopt protocol：success 返
   `ctx.result_slot`；GuardFailed → fallback；Aborted → invalidate +
   fallback。M4 保守版直接 re-run，partial-resume 留 M5。
6. `diff_test_3way` + `tests/three_way_smoke.rs` 11 cases（tw / aot /
   trace 三方对比，arith 路 100 % AllAgree）。

### 10.2 v6-γ M4 Gate

| Gate | 命令 | 结果 |
|---|---|---|
| build | `cargo build --workspace` | ✓ |
| test | `cargo test --workspace` | 1693 passed / 0 failed |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| fmt | `cargo fmt --all -- --check` | ✓ |
| wasm32 | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ |

1654 (M2+M3) → 1693 (M4) = +39 tests。新增覆盖：

- `relon-trace-recorder/tests/orphan_guard_fixed.rs` —— 7 case，确认
  Add/Sub/Mul/Div 的 ArithOverflow guard 都进 `buf.guards` 表。
- `relon-codegen-native/src/trace_recording.rs` —— 8 unit test 覆盖
  const / arith / cmp / local / let / return + div-by-zero + float-
  arith abort。
- `relon-test-harness/tests/trace_jit_smoke.rs` —— 8 个新 case
  （registry round-trip / `invoke_with_fallback` 成功 / no-trace
  fallback / invalidate / save_deopt slot populated / abort on
  unsupported op / install via real helper / no-registration noop）。
- `relon-test-harness/src/three_way.rs` —— 5 unit test。
- `relon-test-harness/tests/three_way_smoke.rs` —— 11 集成 case，三方
  全 `AllAgree`。

### 10.3 性能数字

M4 不涉及 bench。HotCounter prologue 的 ~3-5 ns / warm-invoke 数字
M2+M3 已记录；trace fn 装上后 hot-path 走 trace 而非 generic 的 bench
延后到 M5（需要长跑 entry 才有意义）。

### 10.4 遗留 todo（M5）

- 全 corpus 52-case 三方 differential 跑通。
- Partial-resume from `snapshot.external_pc`。
- 热循环 10^6 iter < 5 ns / iter bench。
- Prologue 真带 args ptr（今天传 null）。
- `TraceHookFn` 加宽版以让 `resolve_call` / `inline_cache_lookup`
  也能进 host hooks table。

---

**Author**: Relon perf 直路 v5-γ stage 2 implementer
**Date**: 2026-05-19
**License**: Apache-2
