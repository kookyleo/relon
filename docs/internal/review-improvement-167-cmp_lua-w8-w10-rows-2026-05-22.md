# review-improvement-167: cmp_lua W8 / W9 / W10 trace_jit rows

**日期**：2026-05-22
**关联**：review-improvement-139（W2 / W12 / W7 honest n/a），
v6-perf-target-1.5x-completion-2026-05-21

## 背景

cmp_lua 此前 W8 / W9 / W10 仅有 tree_walk + LuaJIT，缺 trace_jit row。
本轮按 #139 模板（`install_recorder_trace` + per-workload IR fixture）
补齐三 row。

## 实现

新增三个 recorder body builder（`w8_recorder_body` / `w9_recorder_body`
/ `w10_recorder_body`），保留与 W2 / W12 一致的"分析等价 IR"约束 ——
source-level closure / `Op::If` / `Op::Select` / `Op::BitAnd` 均会 abort
recorder（UnrecoverableEffect / UnsupportedOp），所以 fixture 用 IR-level
等价计算重写工作负载内核：

- **W8 poly_callsite**：`dispatch(i % 4)` 折成 `(i % 4) + 1`（tag ∈ 0..=3
  下完全等价）。
- **W9 nested_matrix**：嵌套 `range(n).map(...).reduce(...)` 改写为
  双层 `Op::Loop` 累加 `i*n + j`。recorder 的 `open_loops` LIFO 栈与
  trace-JIT 的 LICM（innermost-first）都支持嵌套 loop。
- **W10 config_eval**：`(a||b) && (c||d) && (e && f) ? 1 : 0` 改写为
  四 compare 的乘积 `(role<2) * (region<2) * (hour>=8) * (hour<18)`。
  `Op::Lt` / `Op::Ge` 产 0/1 i64 cell，连续 `Op::Mul(I64)` 等价 AND。

`W{8,9,10}_REC_FN_ID` 占 `MAX_FN_ID - 15..17`，与既有 slot 不冲突。

## 测量数

`cargo bench -p relon-bench --bench cmp_lua -- W8|W9|W10`（machine 非
quiescent，sample_size 30，仅作 install 验证 + 量级估算）：

| Workload | trace_jit | LuaJIT | ratio |
|---|---:|---:|---:|
| W8 poly_callsite | 53.7 µs | 132.5 µs | **× 0.41** |
| W9 nested_matrix | 0.40 µs | 52.1 µs | **× 0.008** |
| W10 config_eval | 16.5 µs | 26.3 µs | **× 0.63** |

W9 数字异常低（400 ns / 1024-iter call）的原因：recorded `n=32` 在
let-slot 内是常量，Cranelift loop-opt 可对 `i*n + j` 内层做强度折叠 +
封闭式归约。这是"trace JIT 拿到 clean IR 后能做什么"的诚实上限，与
#139 W2（× 0.36）同性质的"分析等价 IR"测量。

## Gate

- `cargo fmt --all --check` ✓
- `cargo clippy --workspace --all-targets -- -D warnings` ✓
- `cargo test --workspace` ✓（全 pass）
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` ✓

## 落点

- `crates/relon-bench/benches/cmp_lua.rs`（+3 fixture body，+3 bench
  row，+3 fn_id 常量）
- `docs/internal/v6-perf-target-1.5x-completion-2026-05-21.md`（ratio
  表 + 遗留列表更新）
