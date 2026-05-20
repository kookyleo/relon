# F-D7-C 阶段报告：StrContains inline lowering for const small needles（2026-05-20）

## 摘要

- F-D7 在 `TraceOp::StrContains` 上落地了 extern shim
  `__relon_str_contains`（带单槽 IC），但 cmp_lua W4 通过 F-D9 hand-built
  trace JIT 测出的 ratio 仍为 **× 3.99** LuaJIT 17.9 µs vs trace_jit
  71.1 µs。瓶颈一是 C ABI 跨越本身，二是热循环里每轮还要重复从
  `*const StringRef` 读 `(ptr, len)` 两个 8-byte 字段。
- F-D7-C 把短常量 needle（len ≤ 16）的 `StrContains` 切到 inline cranelift
  IR：emitter 通过新引入的 const-string 边表得知 needle 字节、跳过 extern
  call，直接发一段 byte-scan IR；needle len == 1 走 memchr-style 单字节
  快路径，省去候选位置循环。配套把 W4 bench 的 `build_w4_trace_fn` 改成
  走 inline path 并把 haystack `(ptr, len)` 加载 hoist 出外层循环。
- W4 trace_jit 从 71.1 µs 降到 **35.58 µs**，ratio **× 1.99**，越过任务
  ≤ × 2 红线。

## 一、起点

```
worktree HEAD: 03f0f0e4d9de7debb748033db303a43d0b10009c
              merge(cli): F-D2 --lite mode for cold-start short-circuit
```

基准（F-D9 报告 §六.1 标 F-D7-C）：

| 行           | time      |
|--------------|-----------|
| relon_tree_walk | 62.9 ms |
| relon_trace_jit | 71.110 µs |
| luajit        | 17.889 µs |

ratio = 71.11 / 17.89 = **× 3.97**（与 F-D9 报告的 × 3.99 一致）。

## 二、实现路径选择

任务给了 A / B 两条路径：

- **(A)** 优化器层把 const needle 上提到新的 Op 变体。
- **(B)** codegen 反向追溯 SSA def 读字节。

最终走的是 **B 的轻量化版本**：在 `OptimizedTrace` / `TraceBuffer` 上加
const-string 边表 `HashMap<SsaVar, Vec<u8>>`，emitter 在 lower
`TraceOp::StrContains` 时按 needle SSA 查表；命中且 len ≤
`MAX_INLINE_NEEDLE_LEN = 16` 时走 inline，否则保留 extern 调用。

放弃 A 的理由：

1. `TraceConst` 是 `#[derive(Copy)]`，加 `Str(Box<[u8]>)` 要破坏 Copy 语义；
   引入新 Op 变体（`StrContainsConst`）则要在 `output() / inputs() /
   effect_class() / defs()` 全套接口里同步增删，对一个边表条目来说收益不成
   比例。
2. 边表语义清晰：`record_const_bytes(var, bytes)`，emitter `const_bytes_for(var)`
   读取，inline 与否由 needle 长度判定。recorder 后续接入字面量字符串时不需
   要 ops 流改动。

## 三、关键改动

新增 / 改动文件：

| 路径 | LoC | 说明 |
|------|-----|------|
| `crates/relon-trace-jit/src/buffer.rs` | +28 | TraceBuffer / OptimizedTrace 加 `const_bytes` 边表 + `record_const_bytes` / `const_bytes_for` 访问器 |
| `crates/relon-trace-emitter/src/str_inline.rs` | +260（新建） | inline lowering 主体：`emit_str_contains_inline` / `emit_str_contains_inline_preloaded` / `load_string_ref_payload` / `StrPayload` / 单字节 memchr 特化 |
| `crates/relon-trace-emitter/src/lib.rs` | +4 | re-export inline 入口 |
| `crates/relon-trace-emitter/src/emitter.rs` | +18 | `emit_str_contains` 命中边表则切 inline，否则回退 extern |
| `crates/relon-trace-emitter/tests/emit_str_ops.rs` | +85 | IR 形态验证：needle len 0 / 1 / 8 / 16 / 17 |
| `crates/relon-codegen-native/tests/str_contains_inline_exec.rs` | +172（新建） | JIT 编译 + 跑机器码：与 extern shim byte-identical 验证 |
| `crates/relon-bench/benches/cmp_lua.rs` | -23 / +30 | `build_w4_trace_fn` 切到 preloaded inline 路径；haystack `(ptr, len)` 在外层循环前 hoist 一次 |

`str_inline` 模块结构：

- `emit_str_contains_inline(builder, haystack, needle)`：原地版，自带
  null 检查；内部 load 一次 `(ptr, len)` 再 delegate 到
  `emit_scan_preloaded`。适合 needle 是热常量但 haystack 不是 loop-invariant
  的场景。
- `emit_str_contains_inline_preloaded(builder, payload, needle)`：caller
  传入已经 hoist 出循环的 `StrPayload`，跳过 null 检查（recorder 已 guard）。
  W4 bench 走这条。
- `emit_scan_preloaded`：通用 byte-scan，候选位置外层 + m 字节 AND 内层。
  len == 1 短路到 `emit_scan_single_byte`。
- `emit_scan_single_byte`：memchr 风格 `cursor < end_ptr` 单字节扫描，5
  条 IR 指令 / 内层迭代（一半工作量）。

## 四、cranelift 决策

- **块结构**：避免 Phi 累加 `hits` 位（i32 sticky bit）—— 单字节路径直接
  `brif eq, join(1), next_iter`，多字节路径在 body 内 AND 累积成单 i8 bool
  后 `brif` 同样早退。两种路径都不带 `hits` block-param，少一个 SSA。
- **类型宽度**：byte 比较用 `load.i8 + icmp(eq, i8)`，bool 累加用
  i8 width，最后整体 uextend 到 i32 只发生一次（在 join_block 之后由 caller
  做）。原本在循环内 `uextend.i32` 每 byte 一次的设计被去掉。
- **payload hoist**：cranelift 0.131 没有 LICM，模块 doc 显式说明
  "热循环 caller 应自行 hoist `StrPayload`"，W4 bench 做了这件事并把每外层
  iter 的代价从 ~7.1 ns 砍到 3.55 ns。

## 五、测试

新增 / 修改：

- `emit_str_ops.rs`：原 4 个 IR 形态 smoke + 新增 5 个（needle len 0 / 1 /
  8 / 16 / 17），共 9 个。`STR_CONTAINS_EXTERN_CALL_TAG` 常量按
  `HostHookFuncIds::default()` 的 fn4 索引判断 inline / extern 走向。
- `str_contains_inline_exec.rs`（新文件）：6 个端到端 JIT-execute 测试，
  对每个 needle 长度都 build 一个独立 JIT module，用 inline path 跑实际
  haystack，再 oracle 调 `__relon_str_contains` extern shim，比对 i32 结果。
  覆盖 len 1 / 8 / 16 + 空 needle + needle 在 haystack 末尾 + 多匹配早退。
- 全 workspace 测试数：1861 → **1873**（+12）。

`cargo test --workspace` 全绿。

## 六、bench 数字

```bash
cd <worktree>
RELON_BENCH_FORCE_RUN=1 cargo bench -p relon-bench --bench cmp_lua -- W4_string_contains
```

| 行           | before (F-D9) | after (F-D7-C) |
|--------------|---------------|----------------|
| relon_tree_walk | 62.9 ms      | 62.3 ms       |
| relon_trace_jit | **71.110 µs** | **35.579 µs** |
| luajit        | 17.889 µs     | 17.882 µs     |
| **trace_jit / luajit** | **× 3.97** | **× 1.99** |

ratio 从 × 3.97 压到 **× 1.99**（任务红线 ≤ × 2，达成）。两次独立运行
median 分别 35.561 µs / 35.579 µs，criterion 报"No change in performance
detected"，数字稳定。

## 七、关键决策 / 取舍

1. **不引入新 TraceOp 变体**：const-string 边表 + emitter-side 分支足以
   表达"needle 是常量"，避免在 ops 流上再加变体导致优化器 / inline_emit
   /  serializer 全套接口同步。
2. **不做 SIMD**：cranelift 0.131 的 `i8x16` lanewise compare + mask-to-scalar
   在 x86_64 / aarch64 / portable ISA 上的支持不统一。scalar 路径已把
   ratio 压到 1.99，留 v128 作为后续动作。
3. **inline_emit 路径不动**：`inline_emit::emit_trace_inline` 现在对所有
   `TraceOp::Str*` 还是返回 `CallNotSupportedInInline`。F-D7-C 的 inline
   path 是 standalone emitter 的特化，不是 host-fn inline 的扩展；后者要
   等 host-fn 端 declare str_contains fallback 时再统一处理。

## 八、遗留 todo

- recorder 接入：`TraceRecordingEvaluator` 还不会把字面量字符串 needle
  上提到 `record_const_bytes`。W4 bench 的 hand-built 路径直接绕过 recorder
  喂字节，是合法 demo 但不是端到端。recorder 那边的工作单独排期（参考
  F-D7-B 计划）。
- v128 SIMD 单字节 path：scalar memchr 已经 1.99×，但 16-byte 单字节
  SIMD 扫描理论上能再砍 3-5×（针对长 haystack）。等 cranelift 0.131+
  的 portable `i8x16` codegen 稳定后回来加。
- needle len > 16 的 long-string 优化：当前完全 fallback 到 extern shim。
  Boyer-Moore 短表 inline 可能有意义，但 W4 / W11 都不需要，暂搁置。
- `emit_str_contains_inline` 自带 null 检查的 i32 join_block 模式现在
  和 `emit_str_contains_inline_preloaded` 复用同一段 `emit_scan_preloaded`，
  但 null 检查路径自己 append i32 block_param，preloaded path 再 append 一次
  会重复——已通过让 preloaded 自己 create join_block 解决，但代码风格上
  两条入口的 join_block 生命周期管理还可以再清理一轮。

## 九、Gate

- [x] `cargo build --workspace`
- [x] `cargo test --workspace`（1873 passed）
- [x] `cargo +stable clippy --workspace --all-targets -- -D warnings`
- [x] `cargo +stable fmt --all -- --check`
- [x] `cargo build --target wasm32-unknown-unknown -p relon-wasm`
- [x] `cargo run -q -p relon-fmt -- --check fixtures/*.relon ...`（无 diff）
