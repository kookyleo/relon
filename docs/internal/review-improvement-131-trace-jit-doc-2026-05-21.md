# Review improvement 131 — trace-JIT doc 收口 + 小 refactor

Date: 2026-05-21
Branch: `worktree-agent-a1da1059ac3c78bef`
Commits: `5e5f945`, `3d536be`

## 1. Side-table contract 统一 doc

`crates/relon-trace-jit/src/buffer.rs` 顶部新增 "Side-table contract"
模块文档段，覆盖 5 个 SSA-keyed 表（`type_info` / `consts` /
`const_bytes` / `str_payload` / `dict_entry_count_hints`）。同步在
contract 中明确 5 条共享 invariant：(1) SSA-keyed 而非 op-position 索引；
(2) recorder 单写者，`into_optimized` 后只读；(3) optimiser pass 禁止
分配新 SSA；(4) 残留过期 key 无害；(5) `guards` 是 trace_pc anchored，
属于例外（passes 必须同步 rebind）。每个 field 与 `record_*` accessor
追加一行回指 contract 段。`OptimizedTrace` 三个 doc-less field 同步补
doc。

## 2. Optimizer pass ordering invariant

`crates/relon-trace-jit/src/optimizer/mod.rs` 顶 doc 升级为 "Pass
ordering invariants" 段，逐 pass 写明 ordering 依赖与原因（含两轮
`DeadStoreElim`、`dict_ic_hoist` 必须 BEFORE LICM、
`noop_typecheck_elim` 必须 AFTER LICM 等关键约束）。每个 pass 文件
（const_fold, load_forward, dead_store, type_spec, dict_ic_hoist,
licm, noop_typecheck_elim）补 "## Ordering" 段，指向 mod.rs 的统一
契约。`dict_ic_hoist` 原 "Ordering invariant" 段重写以匹配新格式。

## 3. Inline decision pattern 处理（doc only）

评估后选 doc-only 不提取 helper：dict 与 str 的 inline-vs-fallback
决策共享 "probe side table → threshold → tier" 三步 pattern，但
key 类型（u32 entry_count vs `&[u8]` needle）与 inline-form 签名
均不同，`InlineDecisionHelper<T>` 抽象会退化为两个独立 callsite 套壳。
`crates/relon-trace-emitter/src/dict_inline.rs` 顶 doc 新增
"Inline / fallback decision pattern" 段，`str_inline.rs` 镜像补同名段
并互相 cross-ref。dispatch 实现保留在 `emitter.rs`（emitter state
载体），未改动一行 code。

## 4. LoC delta

```
trace-jit (8 文件) : +200 / -27 lines（buffer.rs 主体 +73，optimizer/* 共 +110）
trace-emitter (2 文件) : +66 / -0 lines（dict_inline +37、str_inline +29）
合计           : +266 / -27 (basically 全 doc)
```

无 code 行变更（含未提取 helper）。

## 5. Gate 验证

- `cargo fmt --all --check`：通过
- `cargo clippy --workspace --all-targets -- -D warnings`：通过
- `cargo test --workspace`：2029 passed / 0 failed
- `cargo doc --workspace --no-deps`：通过；trace-jit / trace-emitter
  rustdoc warning 数与改前一致（pre-existing 残留 link，与本次改动
  无关）
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`：通过

## 6. 后续

新 pass 接入时，需同时在 `optimizer/mod.rs` 顶 doc 列表与新 pass 的
模块级 "## Ordering" 段声明依赖。Inline decision 若日后第三个 op 加
入（StrFind / ListGet 等），再评估是否触发 helper 提取。
