# F-D8-E.5 阶段报告：trace emitter 加上 loop preheader hoist + W5 bench 复跑（2026-05-20）

## 摘要

- F-D8-E.4 把 dict_lookup 整个 helper body inline 到 cranelift IR 之后，
  W5 trace_jit 跑分定在 × 1.79（206.29 µs / 115.30 µs）。剩余 gap 不
  在 helper-call 边界上，而在 hot loop 每 iter 重复跑的三类工作：
  - `TraceOp::Mod` 的 divisor-zero 内联 guard（lhs / rhs 都 loop-
    invariant 时本可 hoist）。
  - `TraceOp::ListGet` 的 `list_len: u32` 头部 load + `idx < len`
    bounds compare（`list_ptr` loop-invariant 时 load 可 hoist）。
  - F-D8-E.4 inline 后的 `entry_count = load.u32 [dict_ptr+8]` —
    cranelift 0.131 没把它 lift 到 loop header 之外。
- 本阶段在 trace-emitter 层加了一个「emit-time 的 preheader hoist
  pass」。在 `emit_loop_head` 落 `jump(header)` 之前，先扫描即将
  emit 的 loop body op 流，对每个上述 op 的 loop-invariant 子表达
  式，直接在当前（preheader）block 内 emit 一次，把结果 SSA 缓存
  在 `TraceEmitterState` 的 hoist 边表里。in-loop 的 emit_mod /
  emit_list_get / emit_dict_lookup_inline_with_entry_count 读到
  缓存就跳过对应 per-iter emission。
- 关键的「不要 hoist」决定：`payload_base = list_ptr + 8` 和
  `entries_base = dict_ptr + 12` 都 *不* 提出 loop。两者各自参与
  一个 `idx*8 + (list_ptr+8)` / `scan_idx*16 + (dict_ptr+12)` 的
  address-mode fold —— cranelift 在 hot loop 内能把它折成单条
  x86_64 `lea`。若硬把 iadd_imm 拆出去当独立 SSA，反而打断 fold，
  每 iter 多出一条 `add`，净亏。第一次实现版漏掉这点，bench 直接
  regress +33%（260 µs），第二版只 hoist 真正的 load（`list_len` /
  `entry_count`）后恢复并超出。
- W5 bench rerun：trace_jit 从 **207 µs → 188 µs**（−9%），
  LuaJIT 115 → 114 µs（噪声范围内）。**ratio = 188 / 114 = × 1.65
  （before × 1.79，after × 1.65，−8%）。未达 × 1.5 目标**，诚实
  记录见 §五。
- 改动 3 个文件，+510 / −22 行（不含本报告）。全部 5 项 gate 通过。

## 一、改动文件 + LoC

| 文件 | 说明 |
|------|------|
| `crates/relon-trace-emitter/src/emitter.rs` | 新增 `LoopMeta` + `compute_loop_meta`：扫描 op 流，按 `loop_id` 收集 `(head_pc, back_pc, inside_defs)` 三元组。`TraceEmitterState` 新增 `loop_meta` / `active_loops` 与三个 hoist 边表（`hoisted_list_len`、`hoisted_dict_entry_count`、`hoisted_mod_nonzero_divisor`）。新增 `prehoist_loop_invariants(loop_id)`：在 `emit_loop_head` 落 jump 之前扫 body，对 invariant 子表达式预 emit。`emit_loop_head` / `emit_loop_back` push/pop active_loops；`emit_mod` / `emit_list_get` / `emit_dict_lookup_prechecked` 读缓存跳过。新增 4 条 unit test 锁住 `compute_loop_meta` 的 pc / inside_defs 行为以及 hoist 后 IR 仍 verify。 |
| `crates/relon-trace-emitter/src/dict_inline.rs` | `emit_dict_lookup_inline` 多出 `emit_dict_lookup_inline_with_entry_count(.., Option<ir::Value>)` 变体：在 `scan_init` block 内，若外面传入了 hoisted `entry_count`，直接复用；否则保留原 `load.u32 [dict_ptr+8] + uextend` 行为。`entries_base` 始终留在 inline body 内以保住 `lea`-with-displacement fold。 |
| `crates/relon-trace-emitter/src/lib.rs` | re-export `emit_dict_lookup_inline_with_entry_count`。 |

总计：+510 / −22 行；其中 emitter.rs 大头是 prehoist pass + 4 条 test。

## 二、preheader hoist IR 形态（W5）

`emit_loop_head` emit 出的 IR：

```text
preheader_block:
    n         = load.u64 [args + 0]
    dict_ptr  = load.u64 [args + 8]
    list_ptr  = load.u64 [args + 16]
    # F-D8-E.5 hoist 段：
    list_len  = uextend.i64 load.u32 [list_ptr + 0]
    entry_ct  = uextend.i64 load.u32 [dict_ptr + 8]
    divisor_v = iconst.i64 10
    nonzero   = icmp_ne divisor_v, 0          # 折叠为常量 1
    brif nonzero, ok_b, deopt_block(0, 0)     # cranelift 优化为 unconditional jump
ok_b:
    jump header(phi_inits...)
header(phi_i, phi_acc):
    ...
    # 每 iter 内：
    key_idx     = srem phi_i, 10
    # bounds check 复用 list_len：
    in_bounds   = icmp_ult key_idx, list_len
    brif in_bounds, ok, deopt(0,0)
    elem_addr   = list_ptr + 8 + key_idx*8     # cranelift 折成单 lea
    key_ptr     = load.i64 elem_addr
    # dict scan init 复用 entry_ct：
    scan_loop(0): exhausted = icmp_eq scan_idx, entry_ct
    ...
    entry_addr  = dict_ptr + 12 + scan_idx*16  # cranelift 折成单 lea
    ...
```

## 三、W5 bench

`cargo bench -p relon-bench --bench cmp_lua -- W5_dict_str_key`，
`RELON_BENCH_FORCE_RUN=1`。环境 load1≈3-4（schedutil），LuaJIT 抖
动 1-2 µs 范围内。

| 指标 | F-D8-E.4 baseline | F-D8-E.5 after | Δ |
|------|-------------------|----------------|---|
| trace_jit | 207 µs（206.29） | 188 µs（188.06） | −9.2% |
| LuaJIT    | 115 µs（115.30） | 114 µs（113.94） | −1.2%（噪声） |
| ratio     | × 1.79            | × 1.65            | −8% |

criterion change detection 在 trace_jit 上报 `-25.8% .. -1.2%`
（多 run 平均），其中第一次 perf run 报「Performance has improved」
（p < 0.05），第二次 run（第二版代码错误地多 hoist 了 payload_base）
报「Performance has regressed +35%」—— 那个 regression 让本阶段
明确学到了「load 可以 hoist，address-mode iadd_imm 不要 hoist」
的关键约束。

## 四、Gate 五项

1. `cargo fmt --all -- --check`：通过。
2. `cargo clippy -p relon-trace-emitter --all-targets -- -D warnings`：通过。
   第一遍 hit 了 `collapsible_match` 和 `doc_lazy_continuation`，
   改成 match guard + 改写 doc 段后清掉。
3. `cargo clippy --workspace --all-targets -- -D warnings`：通过。
4. `cargo test -p relon-trace-emitter`：33 个 lib 单元测试 + 集成
   测试全过，含 4 个新增（`compute_loop_meta_collects_body_pc_and_defs`、
   `compute_loop_meta_skips_unmatched_back`、
   `preheader_hoist_emits_invariant_loads_above_loop_head`、
   `preheader_hoist_dedups_loop_invariant_mod_divisor_check`）。
5. `cargo test --workspace --lib` + `cargo test -p relon-test-harness`
   + `cargo test -p relon-bench --test cmp_lua_consistency` 全过；
   W5 / W6 recorder trace + cmp_lua_consistency W5 行（hit value
   与 tree-walk 一致）均通过。

## 五、未达 × 1.5 目标 — 诚实记录

任务目标是把 W5 ratio 从 × 1.79 压到 ≤ × 1.5。本阶段把三个 loop-
invariant 子表达式从 hot loop 提到了 preheader，实测掉到 × 1.65，
距离 × 1.5 还有 0.15。剩余 gap 来源：

- **dict scan loop 主导**：W5 hot path 每 iter 至少做 1 次 FxHash
  over 5-byte key（~8 cycles）+ 1-9 次 entry hash compare（平均
  ~5 次）+ 1 次值 load。这些都不是单纯的 invariant 操作，没法继续
  靠 preheader hoist 砍。
- **`srem` 不可绕**：Mod 的 divisor-zero guard 现在已经提出 loop，
  但 `srem` 指令本身在 x86_64 上是 ~20-25 cycles 的 microcoded 指
  令，无法通过 IR 重写消除。Cranelift 没有「divisor 是常量 10 →
  用 magic-number multiply-high 替换」的 strength reduction（虽然
  这是经典编译器优化）。
- **entry_count = 10 是 record-time 静态信息**：F-D8-E.4 报告里
  已经指出，把 entry_count 作为 per-trace immediate 让 emitter
  unroll 10 次 entry compare 是下一阶段的关键 lever，本阶段没动
  recorder side-table 不在范围内。
- **perfect-hash for 10 keys**：W5 的 10 个 key 是 fixture 固定
  集合，recorder 阶段可以编译出 closed-form `hash & 9 → entry idx`
  跳过线性扫表。也是后续 phase。

要继续向 × 1.5 收敛，下一步候选：
- 把 `entry_count` 进一步当作 per-op immediate，emitter 直接
  unroll 10 次 entry compare（消除 scan_loop 整个循环）。
- 或：recorder 学会针对 ≤16 entry 的小 dict 走 perfect-hash 路径。
- 或：在 trace-jit LICM 这一层把 `ListGet { invariant, variant }`
  / `DictLookupPrechecked { invariant, variant }` 切成
  `Begin{invariant 部分} + Body{variant 部分}` 两个 op，让 LICM
  把 Begin 直接 hoist —— 这样不需要在 emitter 里维护边表。

这些都在 F-D8-E.6 / F-D8-E.7 候选范围内。

## 六、提交

```
perf(trace-jit): F-D8-E.5 hoist Mod / bounds / dict-entry guards
docs(internal): F-D8-E.5 stage report + W5 rerun
```

合并为单 commit 提交。
