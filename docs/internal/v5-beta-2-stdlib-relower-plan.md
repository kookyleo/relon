# v5-β-2 stdlib re-lower 计划（wasm-AOT 退役 → cranelift-only AOT）

> 状态：**设计稿**（2026-05-18）。撰写时 v5-β-1 agent 正在主仓建 `crates/relon-codegen-native`、IR lowering 基础、4 项 sandbox。本文档定 β-2 落地时的"现状清单 + 移植策略 + 风险 + 完工 checklist"，β-2 agent 拿来即用。
>
> 上游：[`wasm-aot-v4-roadmap-sandbox-safe.md`](./wasm-aot-v4-roadmap-sandbox-safe.md) §v5-β。
>
> 下游：[`v5-gamma-cranelift-object-cache-design.md`](./v5-gamma-cranelift-object-cache-design.md)（γ cache 落盘 + dlopen）。

## 目标

β-2 完成时：

1. `crates/relon-codegen-wasm` 整个 crate 在 main 上**消失**。
2. 所有现 stdlib body（在 `crates/relon-ir/src/stdlib.rs` 中以 `Vec<TaggedOp>` / wasm IR Op stream 形式定义）必须有等价的 cranelift IR 移植版本，并通过 differential test 与 tree-walk 后端 bit-identical。
3. `Backend::WasmAot` / `--backend wasm-aot` / `wasm-aot` feature 等公开 API surface 全部清理。
4. wasm-AOT bench 报告（[`wasm-bench-report-2026-05-16.md`](./wasm-bench-report-2026-05-16.md)）在 main 上归档（保留历史 + 加 deprecation note），后续主线 bench 全切到 cranelift native。

## Section 1：stdlib inventory

来源：`crates/relon-ir/src/stdlib.rs::builtin_stdlib()` 返回的 `Vec<StdlibFunction>`。每条 entry 是用 `TaggedOp` 流（wasm IR）手工写的 body。本表覆盖 2026-05-18 时点全部 31 条 entry，按调用频率与复杂度分组。

| # | stdlib name | 体量（粗估行） | 主要 wasm IR Op | 依赖 const 数据表 | cranelift 移植难度 | 风险点 |
|---|---|---:|---|---|---|---|
| **简单算术 / 长度** ||||||||
| 1 | `length(String) -> Int` | 5 | `LocalGet`, `I32Load`, `I64ExtendI32U` | 无 | **S** | string 头部 layout 已固定，零风险 |
| 2 | `list_int_length(List<Int>) -> Int` | 5 | 同上（offset 不同） | 无 | **S** | 同上 |
| 3 | `list_float_length(List<Float>) -> Int` | 5 | 同上 | 无 | **S** | 同上 |
| 4 | `list_bool_length(List<Bool>) -> Int` | 5 | 同上 | 无 | **S** | 同上 |
| 5 | `list_string_length(List<String>) -> Int` | 5 | 同上 | 无 | **S** | 同上 |
| 6 | `list_schema_length(List<Schema>) -> Int` | 5 | 同上 | 无 | **S** | 同上 |
| 7 | `abs(Int) -> Int` | 12 | `LocalGet`, `I64Const`, `I64LtS`, `If/Else`, `I64Sub` | 无 | **S** | overflow 边界（`i64::MIN.abs()` UB），需显式 trap |
| 8 | `min(Int, Int) -> Int` | 10 | `I64LtS`, `Select` | 无 | **S** | 直白 |
| 9 | `max(Int, Int) -> Int` | 10 | 同上 | 无 | **S** | 同上 |
| 10 | `is_empty(String) -> Bool` | 6 | `I32Load` + `I32Eqz` | 无 | **S** | 直白 |
| **memory ops / bump 分配** ||||||||
| 11 | `concat(String, String) -> String` | ~110 | `I32Load`/`I32Store`/`MemoryCopy`, `GlobalGet/Set` scratch cursor, `I32Add` | 无（但用 `__relon_scratch_cursor` global） | **M** | 见 §2 bump allocator 讨论 |
| 12 | `substring(String, Int, Int) -> String` | ~220 | UTF-8 boundary scan loop + `MemoryCopy` + bounds check | 无 | **M** | 多个 trap 出口；UTF-8 不变量必须保留 |
| 13 | `starts_with(String, String) -> Bool` | ~165 | `MemoryCompare` 风格手写循环 | 无 | **M** | 简单逻辑，但要保 early-exit pattern |
| **case folding（简单）** ||||||||
| 14 | `upper(String) -> String` | wrap | 调 `case_fold_body_inner` | `FULL_UPPER_FOLDING`, `CASED_RANGES`, `CASE_IGNORABLE_RANGES` | — | 见 #16 |
| 15 | `lower(String) -> String` | wrap | 同上（不同 table） | `FULL_LOWER_FOLDING` etc. | — | 见 #16 |
| 16 | `case_fold_body_inner` (helper, 用于 14/15/24) | ~1160 (959-2118) | UTF-8 decode loop + 二分查找 + final-sigma context check + bump output write | `FULL_UPPER_FOLDING`, `FULL_LOWER_FOLDING`, `TURKISH_*_FOLDING`, `CASED_RANGES`, `CASE_IGNORABLE_RANGES` | **L** | 最大单体 fn；二分 + state machine；differential test 必须覆盖 final sigma 边界 |
| 17 | `title(String) -> String` | ~90 | UTF-8 + word boundary（whitespace + combining mark）+ 调 upper / lower | + `NON_ASCII_WHITESPACE_RANGES`, `COMBINING_MARK_RANGES` | **L** | 上面的 helper 都齐了才能 lower |
| **locale-aware case folding** ||||||||
| 18 | `upper(String, String locale) -> String` | wrap + locale parse | locale ASCII parse + branch 到 Turkish table | + `TURKISH_UPPER_FOLDING`, `TURKISH_LOWER_FOLDING` | **M** | locale string 解析逻辑直白；分支后复用 #16 |
| 19 | `lower(String, String locale) -> String` | 同上 | 同上 | 同上 | **M** | 同上 |
| 20 | `title(String, String locale) -> String` | 同上 | 同上 + #17 | 同上 | **M** | 同上 |
| **case folding 数据查询 helpers** ||||||||
| 21 | `__casefold_lookup` | ~230 | 二分查找 32-byte encoded entry | `FULL_UPPER_FOLDING` 等的 encoded bytes 视图 | **M** | 注意 encoded layout 在 wasm const pool；cranelift 改走 `&'static` slice |
| 22 | `__is_combining_mark` | ~20 | 调 `range_membership_helper` | `COMBINING_MARK_RANGES` | **S** | range 二分 |
| 23 | `__is_whitespace` | ~140 | ASCII fast-path + range search | `NON_ASCII_WHITESPACE_RANGES` | **S** | ASCII 分支 + range 二分 |
| 24 | `range_membership_helper` (helper) | ~75 | 二分 range table | 由调用方传 | **S** | helper |
| 25 | `range_search_loop_body` (helper) | ~155 | binary-search inner loop | — | **S** | helper |
| **list 高阶函数** ||||||||
| 26 | `list_int_sum(List<Int>) -> Int` | ~120 | length load + iterate + `I64Load` + accumulate | 无 | **M** | overflow 行为统一（trap or wrap），β-2 决定 |
| 27 | `list_int_max(List<Int>) -> Int` | ~170 | iterate + `I64GtS` + select；空 list trap | 无 | **M** | empty trap 出口 |
| 28 | `list_int_map(List<Int>, Closure<Int->Int>) -> List<Int>` | ~170 | iterate + indirect call 闭包 + bump append output list | 无（但依赖 closure descriptor layout） | **L** | indirect call ABI；β-1 cranelift trampoline 必须先 ready |
| 29 | `list_int_filter(List<Int>, Closure<Int->Bool>) -> List<Int>` | ~210 | iterate + indirect call + cond write | 无 | **L** | 同上 + 输出长度运行时决定 |
| 30 | `list_int_fold(List<Int>, Int, Closure<(Int,Int)->Int>) -> Int` | ~120 | iterate + 2-arg indirect call + accumulator | 无 | **L** | 同上 |
| **Unicode normalization（最复杂）** ||||||||
| 31 | `nfd(String) -> String` | wrap | 调 `normalize_body(NFD)` | `NFD_INDEX`, `NFD_POOL`, `CCC_TABLE` | — | 见 #35 |
| 32 | `nfkd(String) -> String` | wrap | 调 `normalize_body(NFKD)` | `NFKD_INDEX`, `NFKD_POOL`, `CCC_TABLE` | — | 见 #35 |
| 33 | `nfc(String) -> String` | wrap | 调 `normalize_body(NFC)` | + `COMPOSITION_PAIRS` | — | 见 #35 |
| 34 | `nfkc(String) -> String` | wrap | 调 `normalize_body(NFKC)` | + `COMPOSITION_PAIRS` | — | 见 #35 |
| 35 | `normalize_body(form)` (helper, 用于 31-34) | ~2050 (3428-5479) | UTF-8 decode + Hangul algo + 二分查找 + CCC canonical reorder + (optional) composition | 全部上面 normalization 表 | **XL** | 单体最大；Hangul 子算法 + canonical reorder 需 in-place stable sort by CCC；composition 阶段 lookahead |
| 36 | `decomp_lookup_helper` | ~185 | 二分 `*_INDEX`，回 pool offset + len | `NFD_INDEX`/`NFKD_INDEX`, `NFD_POOL`/`NFKD_POOL` | **M** | 简单二分 + slice |
| 37 | `ccc_lookup_helper` | ~175 | 二分 `CCC_TABLE`，回 ccc 值 | `CCC_TABLE` | **M** | 同上 |
| 38 | `compose_lookup_helper` | ~240 | 二分 `COMPOSITION_PAIRS` (两 key) | `COMPOSITION_PAIRS` | **M** | 复合 key 二分 |

**统计**：

- 共 ~38 个 fn-level body 单位（其中 14/15/17-20/31-34 是 wrap，复用 helper）。
- 总 wasm IR Op 大约 ~6900 行 stdlib.rs；移除注释 / 测试 / 表查询 helper 后 cranelift IR 移植 line count 预估 ~3500 行（cranelift IR 比 wasm IR 紧凑：扁平 block / br，免去 Block/Loop/End 结构 noise）。
- 依赖的 const 数据表：~330 KB 编码后（`NFD_POOL`/`NFKD_POOL` 各 ~50 KB，case folding 全套 ~80 KB，composition pairs ~40 KB，CCC ~40 KB，rest ~70 KB）。详见 §4 风险讨论。

## Section 2：cranelift IR lowering 策略

### 2.1 通用 pattern

**（a）数据表引用：cranelift `GlobalValue::DataRef` + Rust `&'static`**

现 wasm 后端把每个表 encode 成 byte stream，emit 进 wasm module data section，运行时通过 `i32.load` 在 wasm linear memory 中读。

cranelift 不需要这层间接：

```rust
// 在 Rust 端定义（已经是 pub static 见 §4 红线）
pub static NFD_INDEX: &[(u32, u32, u8)] = &[ /* ... */ ];

// codegen-native 在 emit IR 时引用：
let table_ptr = builder.create_global_value(GlobalValueData::Symbol {
    name: ExternalName::user(0, NFD_INDEX_SYM),  // resolved to &NFD_INDEX[0] at link time
    offset: 0,
    colocated: true,
    tls: false,
});
let addr = builder.ins().global_value(types::I64, table_ptr);
// 二分循环里：let entry = builder.ins().load(types::I32, MemFlags::trusted(), addr_off, 0);
```

收益：

- 表数据**不进入** cranelift code section / module image —— linker / loader 解析为 Rust binary 的 `.rodata` 地址。
- cache 落盘后（γ phase）表数据本来就在 host binary，object 文件只需要 unresolved external symbol，dlopen 时 `dlsym` 接住。
- 改表（如 Unicode 升级）只动 Rust source，cranelift module 不需重 codegen。

**（b）二分查找 helper：内联展开**

现 wasm `range_membership_helper` / `range_search_loop_body` 是单独 wasm fn，调用方走 `Call` 指令。这是因为 wasm 内联代价高（每条 op 都进 IR stream 一次）。

cranelift 不需要：直接在调用 site emit 一个二分循环 block，让 cranelift mid-end 自己做 inlining + LICM。codegen-native 端写一个 Rust helper：

```rust
fn emit_binary_search_u32(builder: &mut FunctionBuilder, table_ptr: Value, table_len: Value, key: Value) -> Value {
    // emit lo/hi loop blocks, return found offset or sentinel
}
```

每个 stdlib body 调用即可。**不要** emit `Call` 到运行时 helper —— 那会把 fn call 开销带回到 wasm 时代。

**（c）UTF-8 decode / encode 循环：直白翻译 wasm 控制流**

wasm body 的 `Block`/`Loop`/`Br`/`BrIf`/`If` 在 cranelift 等价：

| wasm | cranelift |
|---|---|
| `Block ... End` | `builder.create_block()` + 末尾 `jump`/`fallthrough` |
| `Loop ... End` + `Br N` | back-edge `jump` 到 loop header block |
| `Br N` | `builder.ins().jump(target_block, &[])` |
| `BrIf N` | `builder.ins().brif(cond, target_block, &[], fallthrough, &[])` |
| `If ... Else ... End` | 两 block + `brif` + `jump` 汇合 block |
| `BrTable` | `builder.ins().br_table(idx, default_block, table)` |
| `Return` | `builder.ins().return_(&[v])` |
| `Unreachable` | `builder.ins().trap(TrapCode::UnreachableCodeReached)` |

cranelift 控制流是 **flat blocks + terminators**，不带嵌套结构。从 wasm 翻 cranelift 时要先把 wasm 的 structured control flow flatten —— 每个 `Block` 的"end label" 对应一个 cranelift block。这是机械翻译，不烧脑，但 line-by-line 工作量大。

**（d）多返回值**：cranelift 原生 multi-return（IR signature 支持 `returns: Vec<AbiParam>`），比 wasm `multi-value` extension 更直接。Final-sigma helper 等"返回 (cp, advance)" 的辅助 fn 一律 multi-return，少一层 struct pack。

### 2.2 特例

**（a）Bump allocator：从 wasm global 改成 thread-local cursor**

现 wasm runtime 用 `__relon_scratch_cursor` global（i32）+ linear memory 作 bump arena。每次 `concat` / `substring` / `upper` 等 produce 新 String 时 emit `GlobalGet $cursor → I32Add len → GlobalSet $cursor` 三件套。

cranelift native 没有 linear memory；改为 Rust `thread_local!` 一段 mmap 的 buffer + `Cell<usize>` cursor：

```rust
thread_local! {
    static SCRATCH: RefCell<ScratchArena> = RefCell::new(ScratchArena::new());
}

pub extern "C" fn relon_scratch_alloc(len: u32) -> *mut u8 { /* ... */ }
pub extern "C" fn relon_scratch_reset() { /* invoked at end of each run_main */ }
```

cranelift codegen emit `call $relon_scratch_alloc` 取指针 + bounds check 内嵌（arena cap 到 N MiB 时 trap）。**TODO（待 host 决策）：scratch arena 上限定多大？wasm 时代默认 16 MiB linear memory，cranelift 改为 `Box<[u8; N]>` 还是 mmap anonymous？β-1 应已选定，β-2 沿用。**

**（b）trap on bounds：cranelift 原生 `trapif`**

cranelift 支持 `builder.ins().trapnz(cond, TrapCode::HeapOutOfBounds)` —— 一指令完成 cmp + cond br + trap，效率比 wasm `If ... Unreachable ... End` 紧。每次 string slice / list index 前 emit 一次：

```rust
let oob = builder.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, idx, len);
builder.ins().trapnz(oob, TrapCode::HeapOutOfBounds);
```

trap handler 端（Rust signal handler 安装侧）见 [v5-β-1 sandbox spec]（β-1 文档 TBD），转 `RuntimeError`。

**（c）Multi-byte UTF-8 decode：保留 wasm body 同款 branch pattern**

UTF-8 decode 是 stdlib 中 hot path 之一（normalization / case folding 都走它）。wasm body 是手写 4-branch decoder（1/2/3/4 byte sequence）。cranelift 翻译时 **不要** 重新设计算法 —— 1:1 翻译同款 branch + `brif` + `load`，保 byte-for-byte 等价行为。

理由：

- diff test 期间易 isolate bug（输入相同走分支相同）。
- 性能 cranelift 端自动会更好（cranelift 知道 `iadd_imm 0` 等 wasm 不便 fold 的微操作）。
- 后续 v6 trace JIT 出现时 trace recorder 也基于这个 op chain shape 工作。

**（d）Closure indirect call ABI**

`list_int_map/filter/fold` 通过 closure descriptor 调用用户 lambda。wasm 时代 closure descriptor 是 `{fn_table_idx: i32, env_ptr: i32}` 双字，indirect call 走 `call_indirect $type`。

cranelift native 端：closure descriptor 改为 `{fn_ptr: *const fn, env_ptr: *const u8}`，`call_indirect` 走 cranelift 的 `call_indirect` 指令 + ABI signature。**β-1 必须先把 closure / capability / host fn 的 ABI 定死**，β-2 stdlib 移植直接复用。

**TODO（待 host 决策）：closure descriptor 是否保留 wasm 同款 in-arena 布局（让 closure 也分配在 scratch arena），还是改为 host-heap 上 `Arc<ClosureDescriptor>` + 弱引用？前者 cache-friendly，后者更接 Rust 习俗。β-1 应已选定。**

## Section 3：推荐移植顺序

按"依赖少 → 依赖多 + 难度 S → L"排序。每完成一阶就立刻补 differential test，保证 main 持续绿。

### 阶段 P1：算术 + cmp + 控制流验证（β-1 已做，β-2 review pattern）

确认 β-1 选定的 pattern 工作（fn signature mapping / control flow flatten / trap emission）。无新增。

### 阶段 P2：简单 stdlib（覆盖 #1-#10）

- `length` / 5 个 `list_*_length` / `is_empty` — 一律 `load + extend` 模板。
- `abs(Int)` / `min(Int,Int)` / `max(Int,Int)` — 直白控制流。

完工标志：

- cranelift backend 跑 `crates/relon-eval/tests/integration_*.rs` 中所有涉及上述 fn 的 case 与 tree-walk bit-identical。
- 加 differential test corpus 第 1 批（见 §[v6-γ trace JIT design] §4 的 corpus 规划，β-2 应起头）。

### 阶段 P3：memory ops（#11-#13）

`concat` / `substring` / `starts_with`。

完工标志：

- bump allocator 走通（β-1 已 stub，P3 落实 cranelift 端 emit + Rust 端 thread-local arena）。
- substring 的 UTF-8 边界 trap 在 differential test 中精确匹配 tree-walk 的 `RuntimeError::Utf8Boundary`。
- 微 bench：cranelift native vs 当前 wasm-AOT，concat / substring 单条 invoke ≤ wasm-AOT 同档。

### 阶段 P4：case folding 简单 form（#14-#15、#21-#25）

按 helper-first：先 `__casefold_lookup` + `range_membership` 二分 helper，再 `upper` / `lower`。

完工标志：

- 验证 const data ref 跨 crate 工作（`relon-ir` 的 `pub static` 在 cranelift codegen 里通过 `GlobalValueData::Symbol` 解析成功）。
- Differential test 覆盖：ASCII / Latin-1 / 希腊 σ / 全角 / U+0130（土耳其 dotless I 域外）。
- 加 final-sigma context 单测 5+ 用例（这是 wasm 时代 b-5 修过的 regress 点）。

### 阶段 P5：title + list 简单（#17、#26-#27）

- `title` 依赖 #14/#15 + #22/#23。
- `list_int_sum` / `list_int_max` 是 list iterate 模板，map/filter/fold 之前先把 list layout / 遍历 pattern 跑通。

完工标志：

- `title("hello world")` / `title("foo bar baz")` 等 word boundary 在多种 whitespace（ASCII space / U+00A0 / U+2028 / combining mark 后）下 diff test 通过。
- 空 list `list_int_max([])` 触发 cranelift trap → `RuntimeError::EmptyList`。

### 阶段 P6：list 高阶（#28-#30）

`list_int_map` / `list_int_filter` / `list_int_fold` —— 需要 closure ABI ready（§2.2(d)）。

完工标志：

- map / filter / fold 各 5+ 用例 differential test 通过。
- closure capture 跨 stdlib 边界（如 lambda 内访问外层 fn 的 `let` 绑定）行为与 tree-walk 一致。
- 重要：恶意 closure 抛 trap（如越界访问）能正确 propagate 到 host，不破坏 sandbox。

### 阶段 P7：Unicode normalization（#31-#38）

`decomp_lookup` / `ccc_lookup` / `compose_lookup` 先；`normalize_body` 后。

完工标志：

- W3C normalization conformance test suite 100% 通过（β-2 之前 wasm 应已 100%，cranelift 端保持）。
- Hangul 算法（U+AC00..D7AF 范围，algorithmic decomposition + composition）单测 50+。
- Canonical reorder by CCC 在多 mark sequence（`é + diaeresis + acute` 等）下与 tree-walk bit-identical。

### 阶段 P8：locale-aware（#18-#20）

`upper_locale` / `lower_locale` / `title_locale`。依赖 #16 已 ready。

完工标志：

- `tr` / `az` locale 触发 Turkish table；`en` / `de` / `xx` 等不触发。
- 土耳其 i ↔ İ ↔ ı ↔ I 四向 case folding 全套 diff test 通过。

### 阶段 P9：integration + 退役

P1-P8 全过后：

1. Differential test corpus（见 [v6-γ trace JIT design] §4）整体跑过。
2. `Backend::Auto` 路由把 cranelift-native 加成 first-class option，但 wasm-AOT 仍保留作 fallback（β-2 收尾前一周）。
3. 跑 main 上的 perf bench（`crates/relon-bench/benches/wasm_aot_vs_tree_walk.rs` 改名 + 加 cranelift native scenario）。如果 cranelift native warm invoke ≤ wasm-AOT 同档（roadmap 预期 0.3-0.5 μs vs wasm 0.5-1 μs），开始走 §5 退役 checklist。

## Section 4：风险 + Mitigation

### 4.1 数据表 cross-crate visibility

**检查状态（2026-05-18）**：见本文档前置 grep 结论，全部表都已 `pub static` / `pub const`。零改动。

但要警惕：

- 二级 helper（如 `case_folding::simple_upper_folding()` 返回 `&'static [...]`）的 `pub fn` 也要保留，否则 codegen-native 端需要更深的 `pub(crate)` 暴露。
- β-2 中如有新增表，遵从同样规约：定义 `pub static`，附 `pub fn xxx() -> &'static [...]` 访问器（便于将来切到 Sync OnceLock 时 API 不破）。

### 4.2 wasm IR Op 语义模糊点

某些 wasm 行为依赖隐式语义，cranelift 端不一定 1:1：

| wasm op | wasm 行为 | cranelift 等价 | 风险 |
|---|---|---|---|
| `i32.add` / `i64.add` | wraparound on overflow | `iadd`（默认 wraparound） | 一致 |
| `i32.div_s` / `i64.div_s` | trap on div-by-0 + trap on `INT_MIN / -1` | `sdiv` + 显式 `trapnz` | 必须显式 emit trap check |
| `i32.shl` | 取低 5 位 shift count | `ishl`（同样 mask shift count） | 一致 |
| `i32.load offset=N align=A` | 假定 align 是 hint，硬件不强制 | `load MemFlags::trusted()` | 注意 cranelift 默认对 align 不做假设；不要在 codegen 时硬码 `aligned` flag |
| `unreachable` | always trap | `trap(TrapCode::UnreachableCodeReached)` | 一致 |
| `select` | branchless on cond | `select` | 一致 |
| `memory.copy` / `memory.fill` | byte-level copy with bounds | cranelift `memcpy` / `memset` builtin or libc call | β-1 应已定 ABI（建议 libc memcpy via extern call） |

**审计动作**：P3 完工时 cranelift codegen 出第一版后，dump 一份 IR 跟 wasm 文本对照，确认 div / shift / overflow 行为逐项匹配。Diff test 不一定覆盖（除非 corpus 含极端值）。

### 4.3 数据表规模 → cranelift JIT 内存模型

normalization 全套 ~330 KB。cranelift 是否 OK link 这量级 `.rodata`？

- **JIT 模式（β-2 默认）**：cranelift-jit 把代码 emit 到 mmap 的 RX 区，数据 ref 走 `GlobalValueData::Symbol` 解析为 Rust binary 的 `.rodata` 地址（**不复制**）。零额外 JIT 内存压力。
- **AOT 模式（γ 引入）**：cranelift-object emit ELF / Mach-O，数据 ref 写为 unresolved external，dlopen 时 dynamic linker 解析。同样零复制。

结论：330 KB 无忧。但 **TODO（待 host 决策）：是否考虑把 normalization 表 lazy-load？当前是 link-time bundled，binary size 加 ~330 KB。如果担心 binary size 可选 feature flag 拆开。** β-2 默认仍 bundle。

### 4.4 wasm-AOT bench 数据失效

β-2 删 wasm-AOT 后，[`wasm-bench-report-2026-05-16.md`](./wasm-bench-report-2026-05-16.md) 里 wasm-AOT 列彻底从 main 消失。

策略：

1. 在 `wasm-bench-report-2026-05-16.md` 头部加 deprecation note：自 v5-β-2 起 wasm-AOT 退役，本报告作为历史保存。
2. 把当前文件 mv 到 `docs/internal/archive/`，保留 git 历史。
3. 新建 `docs/internal/cranelift-bench-report-<date>.md`（β-2 收尾时写），主 bench 数据切到 cranelift native + tree-walk 双柱。
4. wasm-AOT 数据作为 "historic baseline" 在新报告附录引用（链回归档版本）。

### 4.5 sandbox 4 项不能在移植过程中"暂时关闭"

β-1 已实施 4 项 sandbox（bounds check / trap handler / capability bitmap / deadline）。β-2 移植 stdlib 时不能为了 ship 暂时关闭其中任一项 —— 即便 internal stdlib 信任也不行，因为同一份 cranelift codegen 路径同时服务用户 lambda（不可信）。

具体：

- stdlib body 内 `MemoryCopy` / load / store 一律走 bounds check（哪怕 stdlib 自己知道安全）。理由：trap handler / capability vtable 是 cranelift module 级配置，per-op opt-out 会引入 ABI 分裂。
- normalization body 内 5-8 处二分 + UTF-8 decode 都要 bounds check。性能预算见 [v5-β-1 sandbox spec]：单次 bounds check ~1.5 ns，single fn body 10-50 次 check 总 cost ≤ 100 ns，可接受。

## Section 5：删 `relon-codegen-wasm` crate checklist

β-2 收尾时按以下顺序逐项推。每项一个 commit，便于 reviewer 跟。

### 5.1 Crate 物理删除

- [ ] `crates/relon-codegen-wasm/` 整目录 `git rm -r`。
- [ ] `Cargo.toml`（workspace root）`members = [...]` 摘掉 `crates/relon-codegen-wasm`。
- [ ] `Cargo.toml`（workspace root）`[workspace.dependencies]` 中 `relon-codegen-wasm = ...` 删。

### 5.2 `relon` crate（facade）

- [ ] `crates/relon/Cargo.toml`：
  - 删 `[features]` 中 `wasm-aot = ["dep:relon-codegen-wasm"]`。
  - 删 `[dependencies]` 中 `relon-codegen-wasm = { ..., optional = true }`。
  - 删 default features 中 `wasm-aot`（如果当前默认开）。
  - 审 `wasm-aot-binary` / `wasm-aot-trace` 等子 feature，全删。
- [ ] `crates/relon/src/lib.rs`：
  - 删 `#[cfg(feature = "wasm-aot")]` 全部分支（≈ 12 处）。
  - `enum Backend` 删 `WasmAot` variant。
  - `enum BackendError` 删 `WasmAot(String)` variant + 对应 `#[error]` attr。
  - `evaluator_from_backend()` 删 `Backend::WasmAot => ...` 分支。
- [ ] `crates/relon/src/auto_evaluator.rs`：
  - 删 auto-tier 路由中所有 wasm-AOT 探测 / fallback 路径。
  - `Backend::Auto` 改为只在 tree-walk / cranelift-native 之间选。
  - 修改后跑 `crates/relon/tests/auto_evaluator_smoke.rs`。

### 5.3 `relon-cli` 命令行

- [ ] `crates/relon-cli/src/main.rs`：
  - `enum BackendArg` 删 `WasmAot` variant + `#[value(name = "wasm-aot")]` attr。
  - `--backend wasm-aot` CLI option 文档删除。
  - 删 `if matches!(backend, BackendArg::WasmAot) { ... }` 校验块（line 405 区域）。
  - 删 `WasmAotEvaluator::from_workspace` 调用块（line 382 区域）。
  - `BackendArg::Auto => BackendArg::WasmAot` 默认 fallback 改为 `BackendArg::Native`（β-1 应已加）。
  - `--fuel` 参数与 wasm-aot 绑定的描述更新（如果改名为 deadline-budget）。
- [ ] `crates/relon-cli/Cargo.toml`：删 `relon-codegen-wasm` 依赖（如有直接引）。

### 5.4 测试 / bench / 集成

- [ ] `crates/relon/tests/auto_evaluator_smoke.rs`：
  - 删 `#[cfg(feature = "wasm-aot")]` 测试 fn。
  - 删 assert `!reason.contains("wasm-aot")` 等历史 negative assertion。
- [ ] `crates/relon-bench/benches/wasm_aot_vs_tree_walk.rs`：
  - 改名为 `crates/relon-bench/benches/native_aot_vs_tree_walk.rs`（β-1 应已加 scenario）。
  - 或彻底删 wasm-AOT scenario，仅保 tree-walk + cranelift-native。
- [ ] `.github/workflows/bench.yml`：第 52 行 `BENCH_NAME: wasm_aot_vs_tree_walk` 改为 `native_aot_vs_tree_walk`。
- [ ] `.github/workflows/ci.yml`：grep `wasm-aot` 看是否有 conditional step，无则跳过。
- [ ] 全仓 grep `wasm-aot` / `wasm_aot` / `WasmAot`，剩余命中应**只**在 docs/ 中作为历史引用。

### 5.5 `relon-wasm` browser playground

`crates/relon-wasm/` 是把 Relon 编进 wasm 给浏览器跑 —— 这跟 wasm-AOT backend **是两件事**：前者把 Relon **engine** 编成 wasm；后者把用户脚本编成 wasm 用 wasmtime 跑。前者保留，后者退役。

- [ ] 审 `crates/relon-wasm/Cargo.toml` 不引用 `relon-codegen-wasm`（应当本来就没引；如有，删）。
- [ ] 审 `crates/relon-wasm/src/lib.rs` 不暴露 `Backend::WasmAot`。
- [ ] 浏览器 playground 默认 backend：tree-walk（cranelift native 在 wasm 环境内不可用 —— cranelift 自己也是 native code）。

### 5.6 文档清理

- [ ] [`wasm-aot-status-2026-05-16.md`](./wasm-aot-status-2026-05-16.md) → archive。
- [ ] [`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md) → archive。
- [ ] [`wasm-binary-layout-v1-2026-05-16.md`](./wasm-binary-layout-v1-2026-05-16.md) → archive。
- [ ] [`wasm-bench-report-2026-05-16.md`](./wasm-bench-report-2026-05-16.md) → 加 deprecation note → archive。
- [ ] [`wasm-aot-v3pp-b-2-hash-pinning.md`](./wasm-aot-v3pp-b-2-hash-pinning.md) → archive。
- [ ] [`wasm-adr-*.md`](./)（5 篇 ADR）→ archive（这些是 wasm 时代决策记录，保留作历史）。
- [ ] [`wasm-srcmap-section-v1-2026-05-16.md`](./wasm-srcmap-section-v1-2026-05-16.md) → archive 或重写为 cranelift srcmap design（β-2 时机看是否做）。
- [ ] [`wasm-aot-v4-roadmap-sandbox-safe.md`](./wasm-aot-v4-roadmap-sandbox-safe.md) → **保留**作 active roadmap，加 "β-1 / β-2 落地状态" section 引到本文档。
- [ ] [`wasm-crate-structure-2026-05-16.md`](./wasm-crate-structure-2026-05-16.md) → archive 或更新为 native crate structure。

archive 流程：`git mv docs/internal/wasm-*.md docs/internal/archive/`，更新 `docs/internal/README.md` 索引。

### 5.7 退役 commit 顺序建议（β-2 收尾 1-2 周）

1. `refactor(cli): drop --backend wasm-aot option`
2. `refactor(facade): drop Backend::WasmAot variant + wasm-aot feature`
3. `chore(bench): rename wasm_aot_vs_tree_walk to native_aot_vs_tree_walk`
4. `chore(ci): update bench workflow to native scenario`
5. `refactor(workspace): remove relon-codegen-wasm crate`
6. `docs(internal): archive wasm-* design docs + add deprecation notes`
7. `docs(internal): publish cranelift-bench-report-<date>`

每步独立 reviewable，main 持续可编 + 测试绿。

## 附：差异测试 corpus 入口

β-2 起头时建 `crates/relon-test-harness/`（详见 [v6-γ trace JIT design] §4）。最迟 P3 完工前 corpus 第一批 5+ 用例必须 ready，否则 P4+ 的 stdlib 移植无可信验证手段。

---

**作者**：Relon perf 直路并行 prep 设计稿撰稿 agent
**日期**：2026-05-18
**License**：Apache-2
