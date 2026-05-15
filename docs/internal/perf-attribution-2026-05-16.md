# 堆 alloc 归因报告（2026-05-16）

> 任务：[`perf-plan-draft-2026-05-16.md`](./perf-plan-draft-2026-05-16.md)
> §三 P0 子任务 B —— 用 `dhat-rs` 给 baseline 跑一份**第一份归因报告**，
> 回答 [`perf-baseline-2026-05-12.md`](./perf-baseline-2026-05-12.md)
> "Open questions" 第一条（瓶颈维度到底落在哪一层）。
>
> 本文档定位：**P0 阶段一次性数字快照 + 微优化方向佐证**，不要求长期同步。
> 进入 P1 后用同一 binary 重跑可拿到对照。

---

## 采样方法

- host：`Linux q 6.8.0-110-generic #110-Ubuntu SMP PREEMPT_DYNAMIC
  Thu Mar 19 15:09:20 UTC 2026 x86_64`，桌面级开发机
- rustc：`rustc 1.93.0 (254b59607 2026-01-19)`（与 baseline 同版本）
- profile：默认 `release`（`lto = "fat"` / `codegen-units = 1` / `strip = true`），
  仅本次采样**额外通过 env override** 把 strip 关掉并打开 debuginfo：
  `RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes"`，
  否则 dhat 拿不到调用栈符号。生产 release profile 本身未变更
- commit：`0babcc2ff1ebde3ab2e3c242f5fe0bb2658135fc`
  （`refactor(analyzer): strict mode by default, opt out via #relaxed / #unstrict`）
- 工具：`dhat-rs 0.3.3`，挂 `dhat::Alloc` 作 `#[global_allocator]`，
  整个 `main` 用 `dhat::Profiler::new_heap()` 包住，drop 时落盘
  `dhat-heap.json`（gitignore，不入仓）
- 入口：`crates/relon-bench/src/bin/profile_alloc.rs`，
  feature gate `dhat-heap`。无 feature 时 bin 仍可编译，但只打印提示
- workload（与 baseline 文档同源，便于直接对照 µs/op 数字）：
  - **simple**：`{ val: 1 + 2 * 3 / 4.0 }`，完整 parse + eval 跑 **1000 次**
  - **comprehension**：
    `{ "list": [x*2 for x in range(1000) if x%2==0], "check": &sibling.list }`，
    完整 parse + eval 跑 **100 次**
- 每次 iteration 都重新 `Context::new()` + `with_root` + `Evaluator::new`
  —— 模拟"宿主一次性 eval、不复用上下文"的最坏情况；context 复用本身
  是 P1 候选，混在这一轮归因里会把其它热点盖住

两个 workload 各**独立运行一次**（CLI 参数选择），各自生成一份
`dhat-heap.json`；两份 json 分别按 (allocation site, call stack)
聚合到模块桶。

---

## 总体数字

| workload | iterations | total bytes | total blocks | at-gmax bytes | at-gmax blocks | at-end bytes | at-end blocks |
| --- | --- | --- | --- | --- | --- | --- | --- |
| simple | 1 000 | 60 451 024 | 296 001 | 9 324 070 | 19 187 | 9 285 024 | 19 001 |
| comprehension | 100 | 540 072 710 | 1 553 702 | 36 570 593 | 8 636 | 35 871 724 | 8 401 |

平均到单次 iteration：

| workload | bytes / op | blocks / op |
| --- | --- | --- |
| simple | ≈ 60.4 KB | ≈ 296 |
| comprehension | ≈ 5.4 MB | ≈ 15 537 |

观察：

- **simple 单次 296 个 heap 块**只是为了算一个 `1 + 2 * 3 / 4.0`，
  量级上明显是 "constant-time context boot 的固定开销"（详见下节）。
- **comprehension 单次 1.55 万块、5.4 MB**，按 1000 个元素摊到每个
  comprehension element ≈ 15 块 / 5.4 KB，跟"`HashMap` 一次 insert + `Arc<Scope>`
  一次 child + 多次 `String::clone`"的人工估算同量级。
- comprehension 的 at-gmax 仅 36 MB 但 total 累计 540 MB —— 说明绝大部分
  分配都是 transient（comprehension 结束就释放），**短命对象密度极高**，
  这正是 `Scope::child` / `iter_scope_map.insert` 每元素都来一发的特征。
- at-end 与 at-gmax 接近 —— Context / Evaluator 自身的常驻 ~9 MB / ~36 MB
  内存（stdlib 注册表、prelude schemas）在 main 退出前不会释放，是
  100% 可预期的；不构成"泄漏"。

---

## Top-10 alloc site 归类

按 module 路径分桶，**按 total bytes 排序**，括号内为占该 workload 总 bytes 的比例。

### simple workload（总 60.4 MB / 296 K blocks）

| # | module 桶 | 总 bytes | 总 blocks | 占比 | 代表 site（源文件:行） |
| --- | --- | --- | --- | --- | --- |
| 1 | `relon_evaluator::eval` | 18 850 000 | 107 000 | 31.2% | `eval.rs:1034` BTreeMap insert（dict 字段写入），`eval.rs:321` `Context::register_fn`，`eval.rs:358` `Context::register_method` |
| 2 | `relon_parser` | 29 838 000 | 105 000 | 49.4% | `lower.rs:2395` `lower_dict_v2` Vec push，`lex.rs:30` token vec，`token.rs:296` `Node::clone`（Box\<Expr\> 克隆） |
| 3 | `relon_evaluator::prelude` | 9 762 000 | 31 000 | 16.1% | `prelude.rs:57 / 61 / 75` `build_result` / `build_option` HashMap insert，`prelude.rs:24` `seed_prelude_schemas` |
| 4 | `alloc::boxed`（杂项） | 856 000 | 3 000 | 1.4% | — |
| 5 | `relon_evaluator::stdlib` | 592 000 | 37 000 | 1.0% | stdlib 函数注册时的小对象 |
| 6 | `relon_evaluator::scope` | 384 000 | 7 000 | 0.6% | `Scope::default` / `Scope::child` |
| 7 | `relon_evaluator::value` | 88 000 | 1 000 | 0.1% | — |
| 8 | `relon_evaluator::other` | 80 000 | 5 000 | 0.1% | — |
| 9 | `alloc::vec`（杂项） | 1 024 | 1 | 0.0% | — |

**结论（simple workload）**：

- **49% 在 parser**、**31% 在 evaluator 内部 Context 装配（`register_fn` / `register_method`）**、
  **16% 在 prelude schema 装配**。三者合计 **96%**。
- 真正"算 `1+2*3/4.0`"的算术热路径，alloc 占比可忽略 —— 这条简单表达式
  在 evaluator 几乎零分配，**simple workload 量到的几乎全是 boot cost**。
- baseline 数字里 "simple eval steady 43 µs" 多半也是 boot cost 主导
  —— 这一条单独看不出 evaluator 的瓶颈。

### comprehension workload（总 540 MB / 1.55 M blocks）

| # | module 桶 | 总 bytes | 总 blocks | 占比 | 代表 site（源文件:行） |
| --- | --- | --- | --- | --- | --- |
| 1 | `relon_evaluator::eval` | 418 959 000 | 1 016 000 | 77.6% | `eval.rs:1070` HashMap insert（comprehension 每元素一次 `iter_scope_map`）；`eval.rs:1079` Vec push（comprehension 结果列表 grow）；`eval.rs:1066` materialize_iterable；`eval.rs:1116` call_function arg vec |
| 2 | `relon_evaluator::stdlib` | 64 123 200 | 4 100 | 11.9% | `stdlib.rs:381` `Range::call` 物化 `range(1000)` → `Vec<Value::Int>`（每次 32 MB / 100 块，含 grow 阶梯） |
| 3 | `relon_evaluator::scope` | 48 177 300 | 204 000 | 8.9% | `scope.rs:179` `Scope::child` 每次 `Arc::new(Self {…})`（comprehension 每元素一次）；`scope.rs` HashMap default |
| 4 | `relon_parser` | 7 461 500 | 325 000 | 1.4% | `lower.rs` / `lex.rs`（解析一次摊到 100 次） |
| 5 | `relon_evaluator::prelude` | 976 200 | 3 100 | 0.2% | — |
| 6 | `relon_evaluator::native_fn` | 256 000 | 200 | 0.0% | — |
| 7 | `alloc::boxed`（杂项） | 85 600 | 300 | 0.0% | — |
| 8 | `relon_evaluator::value` | 24 800 | 500 | 0.0% | `Value::list` shrink / Arc wrap |
| 9 | `relon_evaluator::other` | 8 000 | 500 | 0.0% | — |
| 10 | `alloc::vec`（杂项） | 1 110 | 2 | 0.0% | — |

**注**：上表 `relon_evaluator::eval` 桶里**最大的两个 PP 各 139.6 MB**（合计 279 MB / 占整个 workload 52%）
都落在 `eval.rs:1070` —— 那是 comprehension 内层的
`iter_scope_map.insert(id.clone(), item.clone())` + `current_scope.with_locals(iter_scope_map)` 一起的
HashMap 装配。从调用栈深度看，这条线是评估器 hot loop 的"每元素都来一发"，
**单独这一条就值一个专项 P1 改造**（详见下节）。

**结论（comprehension workload）**：

- **77.6% 落在 `relon_evaluator::eval`** —— 集中在 comprehension 内层的
  per-element scope HashMap insert + 结果 Vec grow + 中间 Vec collect。
- **11.9% 在 stdlib `range(1000)` 物化** —— `range` 当前是 eager，
  全部 1000 个 Int 立即装进 `Vec<Value>`；list comprehension 又要再
  collect 一次 → 双重物化。
- **8.9% 在 `Scope::child`** —— `Mutex<HashMap>::new()` × 2（`locals` /
  `thunks`）+ `Arc::new` 整 frame，每个 comprehension element 一次。
- parser 只占 1.4%（解析成本被 100 次 iteration 摊掉）。
- **`relon_evaluator::value` 几乎不出现** —— `Value::Dict` / `Value::List`
  本身的 BTreeMap / Arc<Vec> 装配在 comprehension 路径上只发生一次
  （最终的输出 list），并不主导。

---

## 与计划文档 §二.2 微优化条目的对照

[`perf-plan-draft-2026-05-16.md`](./perf-plan-draft-2026-05-16.md) §二.2 列了
五个 P1 微优化候选，本节给出 dhat 实测对照。

| 计划条目 | dhat 证据等级 | 关键落点 | 说明 |
| --- | --- | --- | --- |
| **字符串 interner（`String` → `Symbol`）** | **充分** | comprehension 顶 #1/#2 各 139.6 MB 的 `eval.rs:1070` HashMap insert（含 `id.clone()` 即 `String::clone`）；`reference.rs:76` `first_name.clone()` 出现 4 次共 ~7.2 MB | comprehension 每个元素一次 `id.clone()`，1000 元素 × 100 iter = 10 万次 `String` 克隆，**这是直接可测的最大单项**。把 `Var(id)` 的 id 改成 `Symbol(u32)` 后这条 site 应整段消失 |
| **`Scope` 锁拆分 / Mutex → frozen Arc** | **充分** | `scope.rs:179` `Scope::child` 占 comprehension 8.9% / 48 MB；每次 `Mutex::new(HashMap::new())` × 2 是不可避免的 alloc | comprehension 每元素都新建一对 Mutex<HashMap>。frozen 后只有第一次 init 需要 alloc，后续走 `Arc::clone`（栈上 cheap） |
| **`BTreeMap` → `IndexMap`（`Value::Dict` 内部存储）** | **证据偏弱** | dhat 数据上 `Value::Dict` 的 BTreeMap insert 在 simple `eval.rs:1034` 占 3.8 MB / 1000 块（6.3%），在 comprehension 上不显著 | dhat 只看 alloc 量，看不出 BTreeMap key-lookup 的 CPU 代价。**这条主要是 CPU 优化项，alloc 端不会大变** —— 等 CPU profile 才能定量。换 IndexMap 的主要收益在字段访问速度，不在内存 |
| **AST clone 去除（`Node::clone` 走 `Arc<Node>` / arena）** | **部分** | simple 上 `token.rs:296` `Node::clone` 出现 3 次共 ~2.7 MB / 3000 块；comprehension 上被解析摊薄（100 iter 共享）后不显眼 | `Node::clone` 主要由 thunk 装配触发（每个 dict field 一次）。在长解析路径或 thunk 密集场景才会成为热点；comprehension 的 thunk 是 1000 个 Int element，但 element AST 是同一个 `Node` 共用 → 现在没事 |
| **`max_steps` 编译期消除分支 / `path_cache_key` 去 `format!`** | **不足** | dhat 没看到 `format!("{}::{}", …)` 的 String alloc 出现在 top-N | 这条主要是 **CPU 分支 + 短字符串 alloc 数量** 的开销，不是字节量；dhat 只能告诉我们字节量。**需 CPU profile 才能确认是否值得改** |

**额外发现（计划文档未列、本次实测浮出）**：

1. **`Context::new()` 在每次 eval 都跑一遍** —— simple workload 96% 的 alloc
   都来自此。这条 **本来不在 §二.2 范围**，因为它是"宿主使用模式"而非
   "evaluator 内部热路径"。但 dhat 数据强烈提示：**P0 完成后应当把
   "Context 复用 / Evaluator 池化" 列为 P1 一阶候选**，对一次性 eval 场景
   收益巨大（≈ 9 MB / context boot）
2. **`stdlib::Range` eager 物化** —— comprehension 12% 的 alloc 都是
   `range(1000)` 物化的 `Vec<Value::Int>`。改成 lazy iterator（只在被
   collect 时物化）能直接抹掉这一桶。**这条 §二.2 也没列**，可以排进 P1
3. **comprehension 结果 Vec 是 `Vec<Value>` push、没预分配** ——
   `eval.rs:1079` 跑了 800 次 grow 共 32 MB（每次 capacity 翻倍）。
   `Expr::Comprehension` 在 iterable 长度可预知时（`range(n)` / 已知
   `List`）应该 `Vec::with_capacity(n)`。**这是 §二.2 也没显式提的小改**，
   单次改动行，回归零

---

## CPU profile 占位

`cargo flamegraph -p relon-bench --release --bin profile_alloc -- simple` 已尝试，
本机 `/proc/sys/kernel/perf_event_paranoid = 4` 阻断了 `perf` 采样：

```
perf_event_paranoid setting is 4:
  -1: Allow use of (almost) all events by all users
>= 0: Disallow raw and ftrace function tracepoint access
>= 1: Disallow CPU event access
>= 2: Disallow kernel profiling
failed to sample program, exited with code: Some(255)
```

按任务约束（不擅自 `sudo`、不动 sysctl）—— **CPU profile 留作离线手工运行**，
待离线 host 上把 `perf_event_paranoid` 调到 ≤ 1 后跑一次 flamegraph，
补在 §"与计划文档 §二.2 对照"标记为"证据偏弱"的两条：

1. `BTreeMap` → `IndexMap` 的字段访问 CPU 占比
2. `path_cache_key` `format!` / `max_steps` 分支检查的 CPU 占比

`cargo flamegraph` 已安装（`/home/l/.cargo/bin/flamegraph`），
RUSTFLAGS 与 release profile 已可生成 debuginfo，离线 host 上直接：

```bash
cd crates/relon-bench
RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes" \
  cargo flamegraph -p relon-bench --release --bin profile_alloc -- comprehension
```

即可得到 SVG。

---

## 复跑流程（reproducer）

```bash
# 一次性跑两个 workload（合并 dhat-heap.json）
cargo run --release -p relon-bench --bin profile_alloc --features dhat-heap

# 分别跑（推荐 —— 归因不串）
RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes" \
  cargo run --release -p relon-bench --bin profile_alloc --features dhat-heap -- simple
RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes" \
  cargo run --release -p relon-bench --bin profile_alloc --features dhat-heap -- comprehension
```

输出文件 `crates/relon-bench/dhat-heap.json`（已 gitignore）。
浏览器打开 https://nnethercote.github.io/dh_view/dh_view.html，
load 该 json 即可交互式查看。

---

## P1 任务排序建议（基于本轮归因）

按 **dhat 实测占比 × 改造代价** 倒排，给 P1 阶段一个**实证排序**
（覆盖 §二.2 原表 + 本轮新发现）：

1. **`Scope::child` Mutex 去除 / frozen `Arc<HashMap>`** ——
   comprehension 8.9% / 48 MB 直接命中，改造范围局限在 `scope.rs`，
   语义改动小（"prepare 后只读"在现有用法中已经成立）
2. **`stdlib::Range` lazy iterator** ——
   comprehension 11.9% / 64 MB 直接命中，新增一个 `Iter`-branded 实现
   即可，无破坏性改动
3. **`Vec::with_capacity` for comprehension result** ——
   comprehension 6% / 32 MB 直接命中，单行改动，零回归风险
4. **字符串 interner（id 路径 → `Symbol`）** ——
   comprehension #1 / #2 共 52% 命中（279 MB），但改动面广（parser / eval / scope 三端联动），
   排第四
5. **`Context` 复用 / `Evaluator` 池化** ——
   simple 96% 命中（这个 workload 不代表生产场景，但任何"一次 boot 多次 eval"
   的宿主都会立即受益）。需要先 API 设计："Context 在多次 eval 间是否
   保持 root immutable / namespace state 隔离"。**先以文档形式定 contract，
   再做实现**
6. **`Value::Dict` BTreeMap → IndexMap** ——
   dhat 端证据不足；等 CPU profile 补齐再排
7. **`path_cache_key` 去 `format!` / `max_steps` 分支编译期消除** ——
   dhat 端证据不足；同上

后续 P1 PR 应保留一次 dhat 对照（同 corpus、同迭代次数）作为回归证据。

---

## P1 进展 + 诊断更正（2026-05-16 追加）

### P1-A 已落地（commit `4413295`）

- 路径：scope-narrow (a) —— `Range` 保持 eager（保 `range(n)` 作 List 的下游兼容性），改 `materialize_iterable` 返 `Cow<'a, [Value]>` + comprehension 结果 Vec 用 `Vec::with_capacity(items.len())`
- agent 自测（atomic-counter bin，非 dhat）：comprehension 1000-iter
  total_bytes **2.66 GB → 2.34 GB，-326 MB / -12.3%**
- 注意：agent 测的 corpus 与 dhat baseline 的 100-iter 不一样（1000-iter ×），
  绝对数字不直接对照；相对比例可信。dhat 复测留给 P1 收尾

### P1-B 已落地（merge `be7db5f`）+ **诊断更正**

> 原归因报告 §"Top-10 alloc site 归类 · comprehension workload" 第 3 行
> 把 `Scope::child` 的 8.9% / 48 MB 归因为 "`Mutex<HashMap>::new()` × 2"。
> **这条结论实测错误。**

P1-B 实施时验证了三个方向：
- 方向 B（`OnceLock<Mutex<HashMap>>` lazy init）：dhat 反向 **+3 MB**
- 方向 A（`Mutex<Option<HashMap>>`）：同样无收益
- `Arc<str>` 顺手改：会扩散到 `crates/relon/` / `crates/relon-lsp/`，
  违反任务范围被否决

最终落地：仅 helper API 重构（`Locals` / `Thunks` 类型别名 +
`locals_for_write()` / `thunks_for_write()` 入口 + `..Default::default()` seam），
dhat 数字与 baseline **完全一致（无收益）**。

**真正归因**：48 MB 来自 `Arc::new(Self {...})` 自身的**调用次数**——
`HashMap::new()` 是零容量、不分配；`Mutex` 是 inline、不分配。
要砍这 48 MB 必须**减少 `Scope::child` 调用次数**（最直接的钩子：
comprehension 内层热循环不要每元素都 `with_local` → `child()`，
而是复用一个外层 frame + 通过 `list_context` 暴露迭代绑定）——
这是**未来 wave** 的工作，超出 P1 本轮范围。

helper API seam 已在 `scope.rs` 留下，为后续工作减少改造面。

### 对原 §"P1 任务排序建议" 的修正

| 原序 | 项目 | 更正后状态 |
| --- | --- | --- |
| 1 | `Scope::child` Mutex 去除 | **判定无效**——alloc 不在 Mutex 上，留 seam 等下一阶段做"hot loop 复用" |
| 2 | `stdlib::Range` lazy | **降级落地为 narrow path (a)**（P1-A，cherry-pick `4413295`），干掉了 `materialize_iterable` 的中转 clone |
| 3 | comprehension Vec `with_capacity` | **已落地**（P1-A 同 commit），cap 来自 items.len() |
| 4 | 字符串 interner | **已落地**（P1-C，merge `60244c5`）；见下节 P1-C 二次诊断更正 |
| 5-7 | Context 复用 / BTreeMap→IndexMap / path_cache_key | 不变，等后续 wave |

### P1-C 已落地 + **二次诊断更正**（merge `60244c5`）

> 原归因报告 §"comprehension workload" 把 "**52% / 279 MB 在 `eval.rs:1070`**"
> 描述为"`id.clone()` × 10 万次 `String::clone`"。**这条聚合视图实测仅对部分。**

P1-C 实施时把 `Locals` / `Thunks` 内部类型从 `HashMap<String, V>` 改为
`HashMap<Arc<str>, V>`、comprehension hot loop 的 `id` 在循环外建一次
`Arc<str>` 之后 refcount-clone，并把 `reference::resolve_variable` 的
length-1 path 跳过 diagnostic Vec 构造。实测结果：

| 指标 | pre-P1-C (`a590bc9`) | post-P1-C (`60244c5`) | Delta |
| --- | --- | --- | --- |
| total bytes | 474.8 MB | 460.7 MB | **-14 MB** |
| total blocks | 1.55 M | 752 K | **-800 K / -52%** |
| at-gmax | 67.5 MB | 67.5 MB | ~0 |

**真实份额拆解**：原 dhat PP 把 `eval.rs:1070` 附近的多个 alloc 聚合
显示成"52% / 279 MB"，但实际拆分：

- `id.clone()` 自身的 String 头：**仅 ~3 MB**（每次 ~32 B × 10w）
- `item.clone()` Value::clone：对 Int 几乎不分配（按值 copy）
- `iter_scope_map` HashMap 桶表 grow：**~272 MB**（680 B / 桶 × 200 K 桶）
- `with_locals` 内的 HashMap 反 insert：剩余几 MB

interner 把 id-clone 和 reference 桶完全清零（**blocks -52%** 是实证收益），
但**字节大头 272 MB 在 HashMap 桶表本身**——受 `Value` enum 宽度
（当前 ~150 B，被 `Closure { Node }` 撑大）约束，需要后续 wave 做
**enum 布局优化或 small-inline locals 表示**才能继续砍。

### 残留可优化项（P2 候选，非 P1 阶段范围）

1. **`Value` enum 宽度收窄**——当前 `Closure { params: Vec<String>, body: Node, captured_env: Arc<Scope> }` 把 enum 撑到 ~150 B；`Schema` / `EnumSchema` 也宽。Box 化大变体让 enum 退回到 ~32 B，HashMap 桶表立省一半（~272 MB → ~64 MB）
2. **comprehension hot loop 复用 Scope frame**（P1-B 诊断的真正目标）——每元素 `with_local` → `child()` → 一个 `Arc<Scope>` alloc，48 MB / 200 K blocks 还在。改造方向：把迭代绑定下沉到 `list_context`，跳过 per-element scope frame
3. **CPU 端 profile**——上述两项的 CPU cost 占比未测，需要离线 host 上把 `perf_event_paranoid` 调到 ≤1 跑一次 flamegraph 确认

---

## 附录 A：dhat 字段速查

- `tb` (total bytes)：该 PP 累计分配字节
- `tbk` (total blocks)：累计分配块数
- `tl` (total lifetime)：累计存活时长（µs）
- `mb` / `mbk`：单次最大 live bytes / blocks
- `gb` / `gbk`：at-global-max-bytes 时刻的 live bytes / blocks
- `eb` / `ebk`：at-end 时刻的 live bytes / blocks
- `fs`：frame index 列表（指向 `ftbl`）
