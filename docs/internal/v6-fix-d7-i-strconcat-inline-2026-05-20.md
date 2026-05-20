# F-D7-I 阶段报告：StrConcat inline lowering for const short rhs（2026-05-20）

## 摘要

- F-D7 落地的 `__relon_str_concat` extern shim 在 W3 string_concat
  trace_jit 里测出 **× 1.61** vs LuaJIT（D-baseline 报告 §0）。瓶颈不是
  单纯的 C ABI 跨越（boundary cost ≈ 6-8 ns/iter），更大的份额在
  shim 内部：两轮 `StringRef::as_str` UTF-8 校验 + `String::with_capacity
  + push_str x2` + `Box<str>` / `Box<StringRef>` 双 Box 分配交接。
- F-D7-I 把 rhs 是已知短常量 (≤ 16 bytes) 的 `StrConcat` 切到 inline
  cranelift IR：emitter 通过 const-bytes 边表得知 rhs 字节、跳过 extern
  call，直接发一段 IR：`load lhs.len → call alloc helper → load
  result.ptr → unrolled store.i8 per rhs byte → return`。alloc helper
  把双 Box 合并成单块 `[StringRef header | payload]` 分配，省掉一次
  malloc / free。
- W3 trace_jit 从 2.21 ms 降到 ~1.94 ms，ratio **× 1.61 → × 1.40**
  （三轮 trimmed mean：1.380 / 1.395 / 1.406；与任务 ≈ × 1.4 目标一致）。

## 一、起点

```
worktree HEAD: 76ae838ef3d8027868c2f34ca7da8217317bf752
              docs(internal): F-D-baseline D1/D5 + full 5-dim snapshot
```

基准（D-baseline 报告 §0）：

| 行              | time          |
|-----------------|---------------|
| relon_tree_walk | 12.30 ms      |
| relon_trace_jit | ≈ 2.21 ms     |
| luajit          | ≈ 1.39 ms     |

ratio ≈ 2.21 / 1.39 = **× 1.61**（与任务给的 × 1.61 一致）。

## 二、实现路径

1. **runtime helper** `__relon_str_concat_alloc(lhs: *const StringRef,
   total_len: usize) -> *mut StringRef`：
   - 单块分配 `[StringRef header | payload bytes]`，header 在前 16
     字节，payload 紧随其后。alloc 走 `std::alloc::alloc` + 手写
     `Layout`，比 `Box<[u8]>` + `Box<StringRef>` 少一次 malloc。
   - 把 lhs payload memcpy 进 buffer 的前 `lhs.len` 字节；剩余
     `total_len - lhs.len` 字节由 JIT 端 fill。
   - 返回 `*mut StringRef`，`.ptr` 字段指向 buffer 的 payload 段。
   - 完全跳过 `StringRef::as_str` 的 UTF-8 校验和 `String` 构造路径。
2. **emitter inline path**
   `relon_trace_emitter::str_inline::emit_str_concat_inline_short_rhs`：
   - 读 `lhs.len`（一次 i64 load）
   - 算 `total_len = lhs.len + rhs.len(iconst)`
   - 直接 `call __relon_str_concat_alloc(lhs, total_len)` 拿 result
   - 读 `result.ptr`（一次 i64 load）
   - 算 `tail_addr = result.ptr + lhs.len`
   - **unrolled `store.i8` 每个 rhs 常量字节**（W3 是 1 字节 → 一条
     `store`）
3. **emitter dispatch** `emit_str_concat` 在三个条件同时满足时切
   inline：host 接好 `str_concat_alloc` FuncId + rhs SSA 有
   const_bytes 边表条目 + 长度 ≤ `MAX_INLINE_CONCAT_RHS_LEN = 16`。
   其余情况维持 extern shim。
4. **recorder const-bytes 边表**：walker
   `TraceRecordingEvaluator::step_str_concat` 和 `step_stdlib_call`
   （`STDLIB_IDX_CONCAT` 分支）观察 rhs 的 `*const StringRef`，
   snapshot 字节进 `OptimizedTrace::const_bytes`；与 F-D7-C 的
   needle 边表共用 storage。

## 三、关键改动

| 路径 | LoC | 说明 |
|------|-----|------|
| `crates/relon-trace-jit/src/runtime/str_ops.rs` | +90 | `__relon_str_concat_alloc` + 4 个单元测试 |
| `crates/relon-trace-jit/src/runtime/mod.rs` | +1 | 重导出新 helper |
| `crates/relon-trace-emitter/src/abi.rs` | +13 | `HostHookId::StrConcatAlloc` + `symbol()` + slot-offset guard |
| `crates/relon-trace-emitter/src/str_inline.rs` | +112 | `MAX_INLINE_CONCAT_RHS_LEN` / `concat_rhs_fits_inline` / `emit_str_concat_inline_short_rhs` + 单元测试 |
| `crates/relon-trace-emitter/src/lib.rs` | +3 | 重导出 inline 入口 |
| `crates/relon-trace-emitter/src/emitter.rs` | +60 | `HostHookFuncIds::str_concat_alloc` 字段 + `TraceEmitterState::str_concat_alloc` + `emit_str_concat` 切 inline |
| `crates/relon-trace-emitter/tests/emit_str_ops.rs` | +110 | inline IR 形态验证：rhs len 0 / 1 / 8 / 16 / 17 / helper 缺席 |
| `crates/relon-codegen-native/src/trace_install.rs` | +18 | declare `__relon_str_concat_alloc` import + register symbol + 填 `HostHookFuncIds` |
| `crates/relon-codegen-native/src/trace_recording.rs` | +45 | `step_str_concat` + `step_stdlib_call::STDLIB_IDX_CONCAT` 记录 rhs const_bytes |
| `crates/relon-codegen-native/tests/str_concat_inline_exec.rs` | +180（新建） | JIT 编译 + 跑机器码：与 extern shim byte-identical 验证 |

总计 ~ +530 LoC（新增 / 修改文件），无删除（保留 extern shim 作 fallback）。

## 四、cranelift 决策

- **单块分配 layout**：`header_size + total_len` 字节的连续块，header
  在前。`StringRef::ptr` 指向 `block + header_size`，与现有 leak-arena
  约定兼容；调用方仍然只看到一个 `*mut StringRef`。
- **unrolled rhs stores**：rhs 常量已知，emitter 发 `iconst.i8 +
  store` 每字节一对；cranelift backend 在 x86_64 上把相邻的字节
  store 折叠成宽 mov（regalloc 自动做）。
- **tail_addr 提前算**：`buf_ptr + lhs.len` 作为公共基址算一次，避免
  16 次重复 `iadd`。store 的 displacement 走 `k as i32` 立即数。
- **早期 short-circuit**：rhs.len == 0 时只 emit alloc + load，不发
  store；rhs.len 超 16 时 emitter 自动 fallback 到 extern。

## 五、测试

新增 / 修改：

| 路径 | 测试 |
|------|------|
| `str_ops.rs` | `concat_alloc_copies_lhs_prefix_and_leaves_rhs_tail_uninit` / `concat_alloc_rejects_null_lhs` / `concat_alloc_rejects_undersized_total_len` / `concat_alloc_zero_total_len_returns_empty_buffer` |
| `str_inline.rs` | `concat_rhs_fits_inline_thresholds` |
| `tests/emit_str_ops.rs` | `str_concat_inline_for_one_byte_const_rhs` / `_sixteen_byte_const_rhs` / `_falls_back_to_extern_for_seventeen_byte_rhs` / `_falls_back_to_extern_without_alloc_helper` |
| `tests/str_concat_inline_exec.rs` | JIT 编译实跑，与 extern shim byte-identical：rhs len 0 / 1 / 8 / 16，跨多种 lhs |

全部通过。`cargo test --workspace --no-fail-fast` 无 failure；`cargo clippy
--all-targets -- -D warnings` 0 warning；`cargo fmt --all -- --check`
clean。

## 六、bench 数据

```
RELON_BENCH_FORCE_RUN=1 cargo bench -p relon-bench --bench cmp_lua \
  -- W3_string_concat
```

| 行              | pre (HEAD `76ae838`) | post (F-D7-I)         | delta            |
|-----------------|----------------------|-----------------------|------------------|
| relon_tree_walk | 12.30 ms             | ≈ 12.30 ms            | 0（未触及 tree-walk） |
| relon_trace_jit | 2.21 ms              | 1.90 / 1.94 / 1.97 ms | −12 ~ −14 %      |
| luajit          | 1.39 ms              | 1.38 / 1.39 / 1.40 ms | 噪声内            |
| **ratio**       | **× 1.61**           | **× 1.38 / 1.40 / 1.41** | **−0.21**     |

三轮采样的 trimmed mean ratio ≈ × 1.40，与任务目标 ≈ × 1.4 一致。

## 七、诚实记录

1. **目标命中但 margin 紧**。三轮 ratio 跨 1.38-1.41，最坏一轮接近
   红线；如果上下文里 luajit 抖到 1.34 ms（峰值约 −3 %），ratio 会回
   到 × 1.42 附近。再砍一个 ns 需要往 SBO / Arc 共享 buffer 方向走，
   不在本 phase 范围内。
2. **boundary cost 不是大头**。任务 brief 估的 "6-8 ns/iter" 是
   extern call 的 ABI 跨越本身；但 W3 每次 concat 还要做 1.1 µs 量
   级的 memcpy + allocator 工作，所以真正穿透的是 **allocator 双 Box
   合并** + **跳过 UTF-8 校验**，不是 inline-vs-call。alloc helper 把
   `Box<[u8]> + Box<StringRef>` 折成单 alloc 是本次 0.2 量级 ratio
   下降的关键。
3. **下一步空间**：W3 仍然 O(N²) memcpy（每次 concat 都全量复制 acc）。
   想拿到 × 1.0-1.2，需要 small-string optimisation（≤ 16 字节内联在
   StringRef header）或 COW 共享 buffer；都是 host-side runtime 改动，
   非 trace-jit 改动面。
4. **未跑 D-dim 综合 ratio**。本阶段只动 `__relon_str_concat` 路径，
   W4 / W5 / W6 / W11 / W12 不应受影响；但全 5-dim 报告留给 D-final
   汇总。

## 八、并发 agent

F-D8-E.4（dict_lookup path）也在 trace-emitter 起手，但只动
`emitter.rs::emit_dict_lookup` 那一段；本阶段动的是 `str_inline.rs` +
`emit_str_concat`，互不重叠。`lib.rs::pub use` 列表可能在
F-D8-E.4 也添加新条目，merge 时按字母序解决即可。
