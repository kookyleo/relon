# F-D8-E.3 阶段报告：LICM hoist ListGet / DictLookup / BoundsCheck（2026-05-20）

## 摘要

- F-D7-D 给 LICM 装上了 `LocalGet` 的 hoist allow-list（W4 ratio × 3.32
  → × 1.66）。F-D8-E.3 沿同一条思路把 `ReadOnly` 集合再扩两类
  （`ListGet` / `DictLookup`），并第一次允许 `Guard(BoundsCheck)` 在所有
  输入 SSA 均 loop-invariant 时被提到 loop preheader。
- 改动**只触动 `optimizer/licm.rs`** 一个生产文件 + 一组 LICM 单测，避开
  E.2 的 emitter inline 路径；trace_ir / recorder / emitter 与 host 接口
  全部保持原状。
- 实测：W5 / W6 的 hot loop 里 `idx` / `key_ptr` 都由 loop 计数器派生
  （loop-variant），按本 pass 的 invariance 判定，新 allow-list 在 W5 / W6
  没有可命中的 hoist 位点 —— 这与任务 brief 里"诚实记录"的预期一致。
  bench 结果同向佐证：W5 / W6 的 trace_jit 与 LuaJIT 同向漂移（load1 由
  7.4 → 10.3，机器整体噪声），criterion 在 W6 / W5-LuaJIT 列报"无统计
  显著变化"，W5 trace_jit 列报 +5.3% 但 W5 LuaJIT 同步 +6.6%，比值
  改善（1.910× → 1.868×）落在噪声内。
- 单元测试覆盖：6 条新增 LICM smoke（invariant ListGet / variant idx
  ListGet / invariant DictLookup / variant key DictLookup / variant
  BoundsCheck / invariant TypeCheck pin），原 10 条 LICM smoke 全部
  保持绿色。
- workspace 5 项 gate（build / test / clippy / fmt / wasm + relon-fmt）
  全部通过。

## 一、改动文件 + LoC

| 文件 | LoC | 说明 |
|------|-----|------|
| `crates/relon-trace-jit/src/optimizer/licm.rs` | +50 / −11 | `is_hoistable` 重写：模块 doc 同步澄清 BoundsCheck 例外；`ReadOnly` allow-list 扩两类；`Guard(BoundsCheck)` 专项分支。`GuardKind` 引入 use。 |
| `crates/relon-trace-jit/tests/licm_smoke.rs` | +245 / −0 | 6 条新增 smoke：`loop_invariant_list_get_lifts_out_of_loop` / `loop_variant_idx_keeps_list_get_inside` / `loop_invariant_dict_lookup_lifts_out_of_loop` / `loop_variant_key_keeps_dict_lookup_inside` / `loop_variant_bounds_check_stays_inside` / `non_bounds_guards_remain_pinned_even_when_invariant`。 |
| `docs/internal/v6-fix-d8-e-3-licm-bounds-2026-05-20.md` | new | 本报告。 |

合计生产改动 **+50 / −11**，测试新增 **+245**。

## 二、关键决策

### 2.1 为什么只放行 `BoundsCheck` 一种 guard

模块 doc 里原本写"Guard placement is position-sensitive"。这条规则的
真实约束是：deopt 触发时 `DeoptState` 期望 trace 已经执行到 guard 的
原位置，下游被 fused 的 `RecoverableWrite` 才能正确回滚。

只要 guard 本身和它前面的所有 ops 都 hoistable，把它整体上移到
preheader 不会越过任何 `RecoverableWrite`，所以"position sensitivity"
退化为对**到 guard 为止的 prefix** 的要求 —— 这恰好就是 LICM 的
invariance 检查（所有 input SSA 在 loop 外定义）。

但即便如此，本 patch 只开放 `BoundsCheck`：

- `BoundsCheck(idx, list_ptr)` 的 pass/fail 完全由两个 i64 决定，
  invariant → iteration-independent。
- `TypeCheck` / `NotNull` 通常已经被 `noop_typecheck_elim` 在
  invariant 情况下 drop 掉，再 hoist 没有 net work saving。
- `ArithOverflow` 引用某个 arith op 的 `dst`，那个 arith op 几乎
  总是 loop-internal；本身不是 hoist 候选。
- `IsZero` 是 BrIf 的对偶，按设计就是 path-sensitive，不能离开
  body。

如果未来 W4 / W5 之外的 case 证明 `TypeCheck` hoist 有收益，会单独
开 follow-up；不在本 patch 范围。

### 2.2 `ListGet` / `DictLookup` 加入 `ReadOnly` allow-list

两个 op 的 effect 都是 `ReadOnly`，与 `Load` 同档。`Load` 没被放进
allow-list 是因为 trace 内可能有指向同一 `(base, offset)` 的
`Store` —— 这是 `dead_store` / `load_forward` 的工作域。但
`ListGet` / `DictLookup` 的状态来自外部数据结构（`Arc<Vec<Value>>` /
`BTreeMap<String, Value>`），trace 自身从不 emit `Store` 去写这些
header；recorder 也不会在同一 trace 内混入 dict / list 的 mutation。
所以这两个 op 的"上次读到的值"与"本次读到的值"在 trace 视角下是
referentially transparent，符合 hoist 安全条件。

### 2.3 W5 / W6 为什么不会因本 patch 提速

把现有 W5 / W6 recorder body 用 invariance 推一遍：

| trace SSA | W5 hot body | W6 hot body |
|-----------|-------------|-------------|
| list_ptr / dict_ptr | LocalGet(2 / 1) → invariant（F-D7-D 已 hoist） | LocalGet(1) → invariant（已 hoist） |
| idx | `i % 10` → loop-variant（依赖 φ） | `i` → loop-variant |
| key_ptr | `keys_list[idx]` → 依赖 idx → loop-variant | — |
| BoundsCheck inputs | `(idx, keys_list)` → 有 variant 项 | `(i, list_ptr)` → 有 variant 项 |
| ListGet inputs | `(keys_list, idx)` → 有 variant 项 | `(list_ptr, i)` → 有 variant 项 |
| DictLookup inputs | `(dict_ptr, key_ptr)` → 有 variant 项 | n/a |

所以 W5 / W6 在新 allow-list 上没有命中点。bench 同向漂移而非分离是
预期行为。

真正能让 W5 受益的是把 `i % 10` 的 ListGet 改写成对**预算好的常量 KEY
索引数组**的 unroll —— 那是 IR 重写而非 LICM。或者像 brief 提示的
"split BoundsCheck：把 list_len load 单独抽 SSA，对 invariant list_ptr
hoist len load"，要求新增 `TraceOp::ListLen` 并修改 emitter 的 inline
bounds-check shape。后者已被 task brief 明确放到 follow-up 域，
本 patch 维持 LICM only scope。

## 三、W5 / W6 bench：before / after

测试环境：开发机（schedutil governor，未严格 quiescent，
`RELON_BENCH_FORCE_RUN=1`）。Baseline 启动 load1=7.36，after-change
启动 load1=10.31 —— **同会话内不可对齐**，机器整体偏热噪声大。

### W5_dict_str_key

| Backend | before (µs) | after (µs) | Δ% |
|---------|-------------|------------|-----|
| relon_tree_walk | 106.45 ms | 106.25 ms | −0.2% |
| **relon_trace_jit** | **222.88** | **234.72** | **+5.3%（criterion 标红 regressed）** |
| luajit | 116.71 | 125.69 | +7.7% |
| ratio (trace_jit / luajit) | **1.910×** | **1.868×** | −2.2%（噪声内） |

trace_jit 与 LuaJIT 同向上漂，比值改善但落在 W5 luajit 的 14 次
"high severe" 离群点造成的噪声里。我**不声称**这是 LICM 带来的
改进。

### W6_dict_num_key

| Backend | before (µs) | after (µs) | Δ% (criterion p-value) |
|---------|-------------|------------|------------------------|
| relon_tree_walk | 66.64 ms | 63.64 ms | −4.5%（p=0.08，无显著） |
| **relon_trace_jit** | **41.18** | **46.92** | **+13.9% middle，p=0.75 无显著** |
| luajit | 125.62 | 184.65 | +47%（!! 6 outliers high severe / 极大噪声） |
| ratio (trace_jit / luajit) | **3.05× faster** | **3.94× faster** | — |

W6 trace_jit 与 luajit 都被 criterion 判为"No change in performance
detected"（p > 0.5）。luajit 列的 +47% 中位漂移有 3 个 high severe
outlier；其 CI [-19.9%, +38.4%] 跨过零，是机器热漂的强信号。

**诚实结论**：本 patch 在 W5 / W6 上无统计上可声明的 ratio 收益，
trace_jit 行的绝对时间在噪声内同向漂移。这与 §2.3 的纯静态分析一致
—— 新 allow-list 没有命中点。

## 四、测试覆盖

| Layer | 测试 | 期望 |
|-------|------|------|
| `licm_smoke::loop_invariant_list_get_lifts_out_of_loop` | unit | invariant `ListGet` + 对应 `Guard(BoundsCheck)` 一起被提出 loop，guard 仍在 ListGet 之前。 |
| `licm_smoke::loop_variant_idx_keeps_list_get_inside` | unit | idx 由 loop 内 `Load` 派生 → `ListGet` / `BoundsCheck` 都不出 loop。 |
| `licm_smoke::loop_invariant_dict_lookup_lifts_out_of_loop` | unit | 两个 ptr 都来自 pre-loop ConstI64 → `DictLookup` 整体 hoist。 |
| `licm_smoke::loop_variant_key_keeps_dict_lookup_inside` | unit | key_ptr 由 loop 内 `Load` 派生 → `DictLookup` 留在 body。 |
| `licm_smoke::loop_variant_bounds_check_stays_inside` | unit | `Guard(BoundsCheck)` 的 idx variant 时不 hoist。 |
| `licm_smoke::non_bounds_guards_remain_pinned_even_when_invariant` | unit | 显式 pin `TypeCheck` 不被 §2.1 的新例外波及。 |
| 原有 10 条 LICM smoke | unit | 全绿，包括 `guard_is_not_hoisted`（TypeCheck）/ `recoverable_write_is_not_hoisted` / nested loop / readonly call / pure call hoist。 |
| `cargo test --workspace` | integration | 134 个 test 文件全绿，覆盖 trace-emitter / trace-recorder / codegen-native / corpus。 |

## 五、Gate（5 项）

| Gate | 状态 |
|------|------|
| `cargo build --workspace` | OK |
| `cargo test --workspace` | OK（134 test target 全绿） |
| `cargo clippy --workspace --all-targets -- -D warnings` | OK |
| `cargo fmt --all -- --check` | OK |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | OK |
| `cargo run -q -p relon-fmt -- --check fixtures/* examples/*` | OK |

## 六、Follow-up

- **`TraceOp::ListLen(dst, list_ptr)` + 拆分 BoundsCheck**：把 list 头
  里的 len 读取抽成独立 SSA，BoundsCheck 的 limit 操作数指向这个 len
  SSA。此后 LICM 在 list_ptr invariant 但 idx variant 的常见 case 下
  能把 len load 提到 preheader，而 cmp 留在 body。需要同时改 recorder
  的 `emit_list_get`、emitter 的 `emit_list_get` 与 `BoundsCheck` 的
  lowering。这是真正能动 W5 / W6 ratio 的杠杆，但跨 4 个 crate，留作
  下个 phase。
- **F-D8-E.2 emitter inline DictLookup**：与本 patch 并行，进一步降低
  hot loop 每 iter 的跨 ABI cost；E.2 不动 licm.rs，可独立 merge。
- **TypeCheck hoist**：若 future profile 显示某 trace 里
  `Guard(TypeCheck)` 在 invariant input 上没被 `noop_typecheck_elim` 
  吃掉，再决定是否让 §2.1 的例外扩到 TypeCheck。
