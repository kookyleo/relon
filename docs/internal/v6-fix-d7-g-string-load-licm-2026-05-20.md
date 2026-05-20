# F-D7-G 阶段报告：LICM hoist for StringRef payload Load（2026-05-20）

## 摘要

- F-D7-C 落地了 `TraceOp::StrContains` 的 inline lowering，并通过
  `HaystackHandle::{Raw, Preloaded}` 给 hand-built W4 bench 一条把 haystack
  `(ptr, len)` 在 loop 外 load 一次的快路径。F-D7-D 让 recorder 自动 lower
  `s + t` / `s.contains(_)`，但 recorder 流目前固定走 `HaystackHandle::Raw`,
  每 iter 还是会 deref 一次 `*const StringRef`。
- F-D7-G 在 trace-jit 的 LICM pass 上扩展 `is_hoistable` 的 ReadOnly 白名单：
  当 `TraceOp::Load(_, base, Offset(0 | 8))` 命中 StringRef ptr / len 偏移、
  且所在循环体里没有任何 `Store` 或 `RecoverableWrite` / `Unrecoverable` 副作用
  op 时，把这条 `Load` 升级为可提升候选。LICM 既存的 input-invariance 闸门
  继续负责区分 base loop-invariant vs loop-carried。
- 阶段目标 W3 string_concat：trace 内根本不出现 per-iter `TraceOp::Load`
  （`acc = acc + lit_a` 只跑 `StrConcat` 一条 extern call），所以 W3 的 ratio
  **保持 × 1.63**（pre 2.2463 ms / post 待回填）。F-D7-G 的实际穿透面是
  recorder 路径的 W4 与未来任何"循环不变 `*const StringRef` 派生 Load"模式 —
  详见下文 §四。

## 一、起点

```
worktree HEAD: 3d7b6149...
              merge(trace-jit): F-D8-E.2 DictLookup IC inline (shape-compare hoist)
```

基线（commit `3d7b6149`，本机 quiescence=non-perf governor + load1≈6.6，
`RELON_BENCH_FORCE_RUN=1`）：

| W3 行           | time         |
|-----------------|--------------|
| relon_tree_walk | 12.391 ms    |
| relon_trace_jit | 2.2463 ms    |
| luajit          | 1.3744 ms    |

ratio = 2.2463 / 1.3744 = **× 1.634**（与任务给的 × 1.60 一致）。

## 二、改动

| 路径 | LoC | 说明 |
|------|-----|------|
| `crates/relon-trace-jit/src/optimizer/licm.rs` | +51 / -6 | 模块 doc §"Hoist eligibility" 加第 5 条；`hoist_one_loop` 预先扫描循环体计算 `body_has_writes`；`is_hoistable` 新签名 `(op, body_has_writes)`；`ReadOnly` 分支加 `TraceOp::Load { Offset(0 \| 8) } if !body_has_writes` 分支 |
| `crates/relon-trace-jit/tests/licm_smoke.rs` | +186 / -7 | 既有用 `Load(_, _, Offset(0))` 合成 loop-variant SSA 的三个测试改用 Offset(24)（落在 F-D7-G 的 hoist 窗口外）；新增 6 个 F-D7-G 测试：StringRef ptr / len Load 提升，loop-carried base 阻断，in-loop Store 阻断，in-loop Div 阻断，LocalGet haystack + 两条 payload Load 同 round 一起 hoist |

合计：**+237 / -13**，单 commit 落地。

## 三、为什么用 LICM 扩展而非新 pass

F-D8-E.2 的 `dict_ic_hoist` 是一个专用 pass，理由是它要"重写"
`DictLookup` → `DictShapeGuard` + `DictLookupPrechecked` 两个 op 的拆分；
新结构是 LICM 看不懂的 schema。

F-D7-G 不重写任何 op，只是放宽 LICM 自己的可提升判定 — 加一条新 pass 等同
于把 LICM 的循环扫描重复一次，再把同样的 inputs-invariance 闸门复制一份。
直接在 LICM 里挂分支可以：

1. 复用既有的 `inside_defs` / `hoist_pcs` 收集和 `rebind_guard_pcs` bookkeeping。
2. 让 LocalGet (F-D7-D 已加白名单) 和 Load 在同一 LICM round 内一起冒出
   循环 — 测试 `local_get_haystack_and_payload_load_hoist_together` 直接
   断言这一点。第二 round 不再需要重扫。
3. 把 "ReadOnly 中可提升的 op 集合" 集中在一个 match 里，方便后续 F-D8 / 
   F-D7 子阶段继续追加。

## 四、安全模型

`TraceOp::Load` 的 effect class 是 `ReadOnly`。F-D8-E.3 给的允许列表只放
了 `LocalGet` / `ListGet` / `DictLookup` 三个，原因是它们对 trace 自身的
写集合"引用透明" — recorder 不会在同一 trace 里向 dict / list payload 头
emit `Store`。`Load` 不能照搬这个论证：它可能读一个普通堆/栈 slot，循环
体里若有 `Store(base, off, _)` / `Div` / `Mod` 等 RecoverableWrite，alias
模型必须假设 `(base, off)` 被改写。

F-D7-G 的闸门策略是**整循环粒度的 coarse alias**：

```text
body_has_writes := any op in loop body matches Store | RecoverableWrite | Unrecoverable
```

`Load` 只在 `!body_has_writes` 时才进入候选集。这条规则对 W3 / W4 / W5 /
W6 已知热路径都正确：

- **W3 (string_concat)**：循环体只有 `StrConcat`（Pure）+ φ 推进 + `Ge` /
  `Add(I64)` 控制流。无写。但热循环里也没有 `TraceOp::Load`，所以闸门开了
  也没东西可提。
- **W4 (string_contains)**：recorder 走 `StrContains`（Pure）+
  `Add(I64)` 累加 + 控制流。无写。haystack 是 `LocalGet` → 一旦 F-D7
  的下一子阶段在 `emit_str_contains` 里把 raw deref 拆成 `TraceOp::Load`
  ptr / len，F-D7-G 直接把它们提到 preheader。
- **W5 (dict)** / **W6 (list)**：同理，循环体目前没有 `Store`；F-D8-E.2 /
  E.3 的 `DictShapeGuard` / `ListGet` hoist 也不会引入写。

如果将来某条 trace 真的在循环体里 emit 了 Store（例如新的 mutable 
let-slot），整个闸门关闭，所有 Load 都留在循环体里 — 保守是安全的代价。

只允许 `Offset(0)` 和 `Offset(8)` 的偏移过滤是另一道闸门：它把 alias 假设
限定在 StringRef payload 头的两个已知字段，未来若引入更多结构体类型可
再补 `Offset(16)`、`Offset(24)`…，每次扩展都强制 review 一次 alias 安
全。这条偏移过滤还顺手保留了 licm_smoke.rs 里既有把 `Load(_, _, 0)` 当
loop-variant SSA 合成器的测试 — 它们改用 Offset(24) 后语义不变。

## 五、闸门 / 测试

```
cargo build --workspace                                            ✓
cargo test --workspace                                             ✓ (no regressions)
cargo clippy --workspace --all-targets -- -D warnings              ✓
cargo fmt --all -- --check                                         ✓
cargo build --target wasm32-unknown-unknown -p relon-wasm          ✓
cargo run -q -p relon-fmt -- --check fixtures/**/*.relon examples/*.relon ✓
```

`licm_smoke` 22/22 通过，含 6 个新增 F-D7-G 测试。

## 六、W3 W4 对照

| W3 行           | pre (F-D7-D)  | post (F-D7-G) | delta            |
|-----------------|---------------|---------------|------------------|
| relon_tree_walk | 12.391 ms     | 14.902 ms     | +20%（噪声）     |
| relon_trace_jit | 2.2463 ms     | 2.4347 ms     | +8%（噪声）      |
| luajit          | 1.3744 ms     | 1.6008 ms     | +16%（噪声）     |
| ratio           | × 1.634       | × 1.521       | -0.11（噪声内）  |

三行 row 全部 "regressed" 同样幅度，criterion 标 `16% high severe outliers` /
`load1=6.85`，全部落在 quiescence 警告里 — 这是 machine noise 主导的回归。
ratio 从 × 1.634 跌到 × 1.521 同样在测量误差范围内。可观察事实：W3 trace
op 流里**没有 per-iter `TraceOp::Load`**（`acc = acc + lit_a` 只跑
`StrConcat` 一条 extern call，剩下的是 LetSet 重绑 / Add(I64) / Cmp /
Br），新 LICM 规则没有可提升的候选 — 改动对 W3 的可观察 codegen 是 no-op。
任务 brief 标的 × 1.4 直接目标与可观察的 W3 op 流不匹配 — 详见 §七 诚实
记录。

W4 行变化也属"未来面"：recorder 仍走 `HaystackHandle::Raw`，
`emit_str_contains` 自己 emit 的 `(ptr, len)` load 不是 `TraceOp::Load`
而是 cranelift builder 直 emit，落在 LICM 看不见的层。把那对 deref
切到 `TraceOp::Load` 是 F-D7-G 的姊妹子阶段（emitter dispatch 改造），
本 round 范围之外。

## 七、诚实记录 / blocked

1. **W3 ratio 没动**。可预见的原因：W3 trace 内无 per-iter `Load`。任
   务 brief 在 "F-D7-D 让 recorder 自动 lower s + t / s.contains(_)，但
   recorder 路径目前走 HaystackHandle::Raw，每 iter 重 load StringRef
   头" 这一段里描述的是 W4 行为，而 brief 顶部又把直接目标钉在 W3 上 —
   两端的 op 流不一样，单靠 LICM 扩展无法穿透到 W3 的 extern-call-bound
   热路径。
2. **真正能影响 W3 的方向**（未在本阶段做）：
   - emit_str_concat 切到一条 inline IR，把 `__relon_str_concat` 改成
     直接 `Arc::clone` + 共享 buffer 重排的 IR；当前实现固定走 extern。
   - 减少每次 concat 的 `String::with_capacity` + `from_owned` 开销；这
     属于 host-side runtime，不是 trace-jit 改动面。
3. **F-D7-G 的真正价值面**是 W4 / W5 / W6 与未来 trace；这一面要等
   `emit_str_contains` 把 deref 露出成 `TraceOp::Load` 才能用上 — 后
   续 F-D7-H（或并发 F-D7-E SIMD memchr 的 follow-up）的工作。

## 八、并发 agent

F-D7-E（needle=1 SIMD memchr）主要动 `str_inline.rs::emit_scan_single_byte`。
F-D7-G 主要动 `optimizer/licm.rs` 和 `tests/licm_smoke.rs`。文件无重叠，merge
应该是无冲突的 fast-forward。
