# Relon WASM 后端设计草案（2026-05-16）

> 草案 / pre-implementation 阶段。本文档**只锁定主契约**，不含 codegen
> 代码。立项前用来对齐共识；进入实施后逐阶段拆分到 ADR / 子设计文档。
>
> 上游事实：B0 阶段（merge `5f3f7eb`）已抽出 `trait Evaluator` 与
> `relon-eval-api` 薄 crate，新后端 impl 该 trait 即可被 host 通过
> `Box<dyn Evaluator>` 调度。本文档假设这一前提。
>
> 范围：把 `.relon` 源码（经 parser + analyzer）AOT 编译为 WASM 字节码
> 模块，与 tree-walker 解释器**共存**而非取代。

---

## 一、四个主契约决策（已锁定）

四个决策都偏"激进 / 与策略文档对齐"的方向，体现"为高频运行时 eval
场景做硬性能"的取舍。

### 决策 1：参数 ABI — Binary memory handshake（Pillar III）

`#main(User u, Cart cart) -> Result<Order>` 在 wasm 模块里编译成：

```wasm
;; Codegen emits the offset table from the #main schema layout:
;;   u.name   @ offset  0   (String, see "string layout" below)
;;   u.age    @ offset 16   (Int, 8-byte slot, 8-byte align)
;;   cart.id  @ offset 24   (Int)
;;   cart.qty @ offset 32   (Int)
(func (export "run_main")
  (param $in_buf_ptr i32) (param $in_buf_len i32)
  (param $out_buf_ptr i32)
  (result i32))  ;; bytes written to out_buf
```

Host 按 schema 提供的 offset 表把所有参数二进制写入 `in_buf`；wasm
直接 `i64.load offset=16` 等指令读。出参对称写入 `out_buf`，返回值是
写入字节数。

**推导子契约**（下一阶段细化）：

- **基本类型 slot 大小固定**：Int / Float 8 字节 8 对齐；Bool 1 字节
  pad 到 8；Null tag 1 字节 pad 到 8。
- **String 布局**：`{ u32 len; u8 bytes[]; }`，inline 存储；变长字段
  紧跟在固定字段之后（按 schema 字段顺序串排）。
- **List 布局**：`{ u32 len; T elements[]; }`，inline；嵌套 List 通过
  指针表（`u32 ptr_offset`）间接索引——避免 fragmentation。
- **Dict（branded）布局**：等价于具名 schema，按字段顺序展平；运行时
  brand 写在 dict 头 4 字节作为 tag。
- **null / Option<T>**：使用 sentinel bit pattern 还是显式 1-byte tag？
  **待定**。倾向 1-byte tag（兼容性更好）。
- **对齐策略**：所有 8 字节 slot 8 对齐；string / list 头 4 对齐。
- **schema 演化**：增加字段必须追加在末尾；删除字段保留 slot 占位
  （breaking-change 标识）。

**Host 端配套**：`relon-eval-api` 提供 `SchemaLayout::offsets(&Schema) -> OffsetTable`
+ `BinaryBuffer::write_at(offset, &Value)` helpers，让 host 不用手算
offset。

**为什么选这条**：策略文档 Pillar III 的承诺就是"宿主把数据按 layout
写好，wasm 零拷贝读"。任何 JSON-based MVP 都是包袱——一旦发布出去就
有 backward compat 顾虑；上来就走 binary handshake 一次到位。

**代价**：MVP 时间长 1-2 周；schema 演化策略要先想清楚（不能边写边补）。

---

### 决策 2：stdlib — 全编进 wasm（self-contained）+ check_cap opcode

`core/string.relon` / `core/list.relon` / `core/dict.relon` /
`core/iter.relon` 及未来 stdlib 全部一起 lowering 到 wasm bytecode，
**每个 wasm module 自包含 stdlib**。Capability gate 通过 codegen 插入
`call $check_cap; br_if 0; trap` 指令实现。

```wasm
(func $check_cap (param $cap_bit i32) (result i32)
  ;; reads from host-imported globals or a pinned linear-memory
  ;; bitmap. returns 1 if allowed, 0 if denied.
)

;; before any fs/net/clock op:
(call $check_cap (i32.const CAP_READS_FS))
(br_if 0)  ;; trap (or returns Err to host)
(call $native_fs_read ...)
```

**推导子契约**：

- **stdlib bytecode tree-shaking**：默认编全集（约 +50-200 KB）；
  优化 mode 走 dead-code elimination 只保留实际引用的 stdlib 函数。
  优先级：先把 full bundle 做对，dead-code 之后再说。
- **capability bitmap**：host 在 instantiate 时通过 import 提供
  `cap_grants: u64`（每个 cap 1 bit），wasm 读这个 bitmap 做 check。
  这样不用每个 native fn 都做 host trampoline。
- **`max_steps` / `max_value_elements` 软约束**：通过 wasm 全局
  decrement counter 实现；溢出时 trap。
- **native fn 仍走 host import**：`fs_read` / `net_get` / `time_now`
  这些**真正**碰外部资源的 fn 必须走 import 回调（wasm 自己做不到）；
  check_cap 是 stdlib 内部的纯计算 gate。host enforce 再多一层防御。

**为什么选这条**：

- FFI 静默：stdlib 内部全 wasm value stack 跳转，没有跨 wasm/host
  trampoline 开销。post-P2 后 method dispatch 已经 -15% wall time；
  wasm 后端不能反向回到 trampoline-heavy 模型
- 部署单文件：一个 `.wasm` 就能跑（除了 import 的 native fn）
- 与 Pillar I 一致：策略文档承诺的"通过宿主 wasm JIT 推上 LLVM 优化"
  只在 stdlib 本地化时才能让 JIT 看见整体

**代价**：

- wasm module 体积膨胀，每个 +50-200 KB。如果用户场景是"很多小 wasm
  模块共存"，需要 component model（决策 2-alt）作为后续 phase
- check_cap opcode 在每个 native fn 调用前都加一条指令；总开销约
  1-2% wall time（实测后确认）

---

### 决策 3：错误回溯 — wasm custom section 内嵌 srcmap

每个生成的 wasm module 带一个 custom section（名字暂定
`"relon.srcmap"`），存 `pc → TokenRange` 表。运行时 trap 时 host
runtime 通过 wasm-tools / wasmparser 读 section 拿源码位置。

```text
wasm module layout:
  type section
  function section
  code section          ← user + stdlib bytecode
  data section          ← consts + binary layout sentinels
  custom "relon.srcmap":
    file_table: ["lib/utils.relon", "main.relon"]
    entries: [
      { pc=0x0042, file_idx=1, line=7,  col=3,  range_end=0x0067 },
      { pc=0x0067, file_idx=1, line=12, col=11, range_end=0x00a3 },
      ...
    ]
  custom "relon.version":
    { codegen_version: 1, abi_version: 1, ... }
```

**推导子契约**：

- **section 格式自定义**：不复用 DWARF（重型 + 工具链不必要）；用
  紧凑二进制编码 (varint pc + line + col)；尺寸预估 ~10% 代码段
- **host runtime 库职责**：`relon-wasm-runtime` crate（新）提供
  `WasmModule::trap_to_diagnostic(trap) -> RuntimeError` 把 wasm trap
  按 pc 翻译回 `RuntimeError { ..., range: TokenRange }`，与 tree-walker
  错误形状一致——`eval_api::Evaluator` trait 不知道后端
- **section 是 strict optional**：wasm-opt 等工具默认保留 unknown
  custom section；如果生产环境 strip 了 section，runtime 只能返回
  byte-offset 错误（向下兼容）

**为什么选这条**：

- 单文件部署，比 sidecar `.smap` 简单
- 不依赖 DWARF 工具链
- standard wasm 工具能 round-trip（wasm-opt / wasm-validate 默认保留
  unknown custom section）

**代价**：

- 需要自己写 reader（`wasmparser::Payload::CustomSection`），约 100-200 LOC
- 体积膨胀约 10% 代码段；用 varint 压缩后预估 5-8%

---

### 决策 4：lazy thunk — 静态拓扑排序 + eager

`Expr::Dict` 的字段依赖在 codegen 阶段做 DAG + topo sort，输出 eager
求值序列。运行时无 thunk 状态机、无延迟求值。Cycle 在 codegen 阶段
检出，报带 TokenRange 的 compile-time error。

```wasm
;; dict { x: a + b, a: 1, b: 2 } 中，依赖关系：
;;   x -> {a, b}
;;   a -> {}
;;   b -> {}
;; 拓扑序: [a, b, x]
;; codegen emits:
i64.const 1                  ;; eval a
local.set $field_a
i64.const 2                  ;; eval b
local.set $field_b
local.get $field_a           ;; eval x
local.get $field_b
i64.add
local.set $field_x
;; build output struct from [field_a, field_b, field_x]
```

**推导子契约**：

- **codegen 期的 cycle detection 是新的 phase**：当前 analyzer 没有
  cycle detection；wasm codegen 阶段第一次正式实施。结果用统一的
  `CodegenError::CircularFieldDependency { fields: Vec<(name, TokenRange)> }`
  返回；前端 driver 把它当 compile error 显示
- **闭包内的引用仍走 capture snapshot**（B0 阶段 P2-B 已落地）：
  closure body 内 `&sibling.foo` 在闭包构造时 snapshot 入 captured
  env，运行时纯查表，与 eager 模型一致
- **`#default` 字段**：默认值在 codegen 阶段被替换；不进入 dict 求值
  序列
- **跨 dict 的 `&sibling` / `&uncle`**：依赖 walk 跨 dict 边界时按现有
  AST 拓扑（外层在内层之前）处理；本质上还是同一个 DAG
- **未知 / 动态 key（`a[expr]`）**：codegen 期标"unresolvable"，把那
  一段 dict 降级到"按声明顺序 eager"（不做 topo 优化）。这种 dict 内
  cycle 检测改为 runtime trap（罕见 case）

**为什么选这条**：

- bytecode 极简 + runtime 零状态 → 性能上限最高
- cycle 提前到 compile time，diagnostics 比 tree-walker 还好（tree-walker
  当前 cycle 只在 eval 时炸）

**代价**：

- 失去"thunk 表"作为运行时 introspection 入口（host 拿不到 partial
  eval state；目前看不需要）
- 用户的高阶"有条件引用"场景如果出现，需要 fallback——目前
  fixture 库里没有这种 pattern，但等 wave 1 实测覆盖

---

## 二、待定子问题（决策 1-4 已锁，但这些没问到）

### A. Closure 值能不能跨 host↔wasm 边界

- **倾向**：不能。Closure 是 evaluator 内部状态；host 只能传 / 收
  data values（Int / Float / String / List / Dict / Null / Bool / 已
  brand 的 typed dict / Enum variant）。`#main` 签名禁止 Closure
  作为入参 / 返回类型（已在 analyzer 强制）。这条 wasm 后端继续遵守
- **副作用**：高阶函数（`xs.map(closure)`）在 wasm 内运行没问题；
  跨边界要传递行为只能通过 schema-rooted method 或 host import

### B. 用户 host fn 的 schema 怎么编进 wasm

- 当前 `Context::register_fn(name, gate, func)` 是动态注册
- wasm 模块编译时不知道 host 将注册哪些 fn
- **倾向**：wasm 模块为每个 `unresolved free fn` 生成一个 import 占位
  （`import "host" "fn_name"`）；host 在 instantiate 阶段 link 实际
  实现。Capability gate 同样走 host import
- **代价**：wasm 模块需要静态分析期就知道"哪些 fn 是 host-provided"——
  analyzer 已经有这个信息（unresolved at analyze time + 不属于 stdlib
  = 必须 host-provided），可直接复用

### C. 多文件 `#import "lib/utils"` 怎么 lowering

- 静态拓扑：所有被 import 的文件 codegen 期就 inline 进同一个 wasm
  模块（"全静态 lowering"）
- 或：每个 module 单独 wasm + component-model link
- **倾向**：MVP 走静态 inline；component model 进 future phase
- **代价**：单 module 体积进一步膨胀；但因 stdlib 已经 50-200 KB 是大头，
  用户 lib 通常小

### D. Schema 验证在哪一侧做

- host 把数据按 binary layout 写入 `in_buf`，wasm 直接 load
- 如果 host 写的字节不符合 schema（如 String 长度字段对不上）：
  wasm 内 load 时**不验证**，UB 风险
- **方向 1**：wasm 入口先 validate `in_buf` 符合 schema（额外开销，
  ~微秒级）
- **方向 2**：host 端 Rust API helper 强制类型安全，wasm 假定 host 写
  对了（trust host）
- **倾向**：方向 2，host SDK 把 binary 写入 typesafe；wasm 不重复验证。
  这与 capability gate 的"信任 host 配置 caps_grants bitmap"思路一致
- **风险**：host SDK 必须够好用，否则手写 binary buffer 容易出错

### E. Wasm runtime 工具选择

- wasmtime / wasmer / wasm3 / 浏览器内置 V8 都能跑
- host crate 选哪个作默认运行时？
- **倾向**：facade 不绑定具体 runtime，提供 `WasmAotEvaluator::new(wasm_bytes, runtime: impl WasmRuntime)` 形态；缺省提供 `wasmtime`
  wrapper（功能最全 + 服务端通用）；浏览器端用 JS API 直接跑
- 这是后续 phase 的事，不阻塞 codegen

### F. wasm AOT vs JIT

- 决策："wasm AOT 编译生成 .wasm 字节码模块"，与"运行时 JIT 编译
  Relon 源码到 wasm 然后跑"不同
- **倾向**：先做 AOT（典型部署）；JIT mode 是后续 phase
- AOT 形态：`relon-wasm-codegen` crate 暴露
  `compile(analyzed: &AnalyzedTree) -> Result<WasmModule, CodegenError>`

---

## 三、阶段计划（spec → code → ship）

每个 phase 都按"小可发"切割，期间 tree-walker 继续是 default backend。

### Phase 0：spec 完善 + 内部 ADR（1-2 周）

- 把上面"待定子问题" A-E 各自展开为 ADR-level micro-doc
- Binary layout v1 spec（offset 计算、对齐、string / list / null 表示）
- Custom section v1 二进制格式
- 决议：单 crate `relon-wasm-codegen` vs 拆 `relon-bytecode-ir` +
  `relon-wasm-codegen-from-ir`（推 IR-first，便于未来 wasm-codegen
  /native-codegen 共享）

### Phase 1：smoke test（2-3 周）

scope 严格：

- `#main(Int x) -> Int : x * 2` 这种最小程序
- 无 stdlib（不调任何 native 函数）
- 无 schema validation
- 无 string / list / dict（仅 Int / Float）
- 无 #default / #expect / #brand / decorator
- 输出 `.wasm` + 用 wasmtime 跑通

目标：验证 trait Evaluator 的 dyn dispatch、binary layout、wasm 字节码
生成、host runtime 调用、错误回传整条链路。

### Phase 2：schema-typed binary handshake（2-3 周）

- `#main(User u) -> ...`，User schema 含 String / Int / Float / Bool /
  Null 字段（基本类型集合）
- Binary layout v1 完整实现：String inline、null tag、对齐
- Host SDK `BufferBuilder` typesafe API
- 错误回传：codegen 期 + runtime trap 都能返 `TokenRange`

### Phase 3：dict + topo eager + cycle detection（2 周）

- `Expr::Dict` codegen 走 DAG 排序
- Cycle detection 报 `CodegenError::CircularFieldDependency`
- 跨 dict 的 `&sibling` / `&uncle` walk

### Phase 4：stdlib bytecode 全集（3-4 周）

- 把 `crates/relon-evaluator/src/std_relon/*.relon` 全 lowering 到
  bytecode 内联
- 实现 List / Dict 的 binary layout（含变长）
- comprehension codegen
- Iter（B0 时已经有 lazy iterator pattern in tree-walker，wasm 走 eager
  化等价语义）

### Phase 5：closure + schema-rooted method dispatch（3-4 周）

- closure value 在 wasm 里 = funcref + captured env buffer
- `xs.map(c)` codegen 调 funcref
- schema method 编译为 wasm function

### Phase 6：capability + native fn import（2 周）

- check_cap opcode emission
- host import boundary
- sandbox semantics 与 tree-walker 等价（同样的 Capabilities bitmap
  含义）

### Phase 7：错误回溯 + custom section srcmap（1 周）

- codegen 期把 pc→TokenRange 写入 custom section
- `relon-wasm-runtime` crate 提供 trap → RuntimeError 转译

### Phase 8：integrate 进 facade（1 周）

- `relon::WasmAotEvaluator` 通过 trait 暴露
- host 一行切换：
  ```rust
  let ev: Box<dyn Evaluator> = if want_perf {
      Box::new(WasmAotEvaluator::compile_and_load(&analyzed)?)
  } else {
      Box::new(TreeWalkEvaluator::new(ctx))
  };
  ```

### Phase 9：bench + 对照 tree-walker（1-2 周）

- criterion + dhat + flamegraph 全套跑过两个 backend 的对照
- 决策：是否替换 default backend（很可能不替换；保留 tree-walker
  作 dev/LSP/REPL 路径，wasm 作 prod-runtime 路径）

**总估算**：18-25 周，约 5-6 个月。这是 single-developer 估算；多人
分 wave 可压缩到 3-4 个月。

---

## 四、风险登记

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| Binary layout v1 设计错，发布后要 breaking change | 高 | Phase 0 多做 paper review，开 ABI version 字段为以后兼容做准备 |
| stdlib 内联让 wasm 模块体积爆炸（>500 KB） | 中 | 实测 wave 1 后跑 size check；如果 >500 KB 提前进 dead-code elimination phase |
| Codegen 期 cycle detection 假阴/假阳 | 高 | 把现有 tree-walker 的 runtime cycle 检测当 oracle 跑 fixture diff |
| Custom section 被 wasm-opt 剥掉 | 中 | 在 build pipeline 里禁掉 wasm-opt 默认的 strip-debug 行为；CI 加 srcmap 存在性测试 |
| 用户实际把 Closure 作为 #main 参数 | 中 | analyzer 已 ban；wasm codegen 再次 enforce + 显式错误 |
| schema 演化 backward compat 设计欠缺 | 中 | ABI version 字段；host SDK 在加载 wasm 时检 version 不匹配就 reject |
| wasmtime / wasmer ABI 差异 | 低 | facade 不绑定 runtime；先做 wasmtime |
| 单 developer 时间预算 5-6 个月内可能拖到 8 个月 | 中 | 阶段计划本身严格切割，每阶段都可独立"小可发" |

---

## 五、不在此 spec 范围（明确排除）

以下内容显式不在本设计的范围内，避免 scope creep：

- **运行时 JIT mode**（`dart run` 那种，源码直接跑同时 tier-up 到
  AOT）—— 后续 phase 单独立项
- **多 backend tier-up**（一个进程内同时跑 tree-walker + wasm）——
  现在的 facade 就是 host-pick-one，足够
- **跨 module link with component model**——MVP 静态 inline 已足够
- **DWARF 完整调试器支持**——决策 3 选的是 minimal custom section；
  完整 LLDB 集成留给"高级调试"项目
- **wasm SIMD 指令利用**——Phase 9 bench 后再看
- **wasm GC proposal 利用**（用于 List / Dict heap-managed 表示）——
  生态尚在演进，本 spec v1 走 linear-memory 路线

---

## 六、Decision sign-off checklist

立项前需要确认（本文档收尾）：

- [ ] Q1 binary handshake 决策被 stakeholder 认可（已锁）
- [ ] Q2 stdlib self-contained 决策被认可（已锁）
- [ ] Q3 custom-section srcmap 决策被认可（已锁）
- [ ] Q4 static topo eager 决策被认可（已锁）
- [ ] 五个"待定子问题" A-E 各自 ADR 完成（Phase 0 输出）
- [ ] Binary layout v1 文档完成（Phase 0 输出）
- [ ] Custom section v1 文档完成（Phase 0 输出）
- [ ] Phase 1 smoke-test 完整 PR review

只在上述全部就位后进入 Phase 1 codegen 实施。

---

## 附录 A：上游事实（B0 阶段 trait Evaluator 抽出）

WASM 后端实施前，B0 阶段已落地：

- `crates/relon-eval-api/`：`trait Evaluator` + 共享类型（Value / Scope /
  Thunk / Context / Capabilities / RuntimeError / ...）
- `crates/relon-evaluator/`：`TreeWalkEvaluator` 是 `Evaluator` 的当前
  实现
- B0 commit 链：`b20ed13` → `5905969` → `94f8e40` → `5f3f7eb` →
  `14439e9` → `cbc3841`
- gate：`cargo test --workspace` 1003 tests / 0 failed；
  `cargo clippy --workspace --all-targets -- -D warnings` clean；
  `cargo build --target wasm32-unknown-unknown -p relon-wasm` ok

WASM 后端 = 第二个 `impl Evaluator`，新建 crate（暂名）
`relon-wasm-codegen`。Frontend / Context / NativeFn 协议全复用，零分裂。
