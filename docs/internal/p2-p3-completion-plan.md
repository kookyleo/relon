# 设计:P2(llvm-AOT)/ P3(wasm)完成计划 —— 一发射器,两目标

- **状态**:计划已定稿,开工中(2026-06-03)
- **关联**:[`adr-execution-tiers.md`](./adr-execution-tiers.md)(3 支柱)、[`tiers-explainer.md`](./tiers-explainer.md)
- **目标**:把 P2(llvm→原生二进制)和 P3(wasm→浏览器/沙箱)做成**完整、独立可运行**的后端线;它们各自场景里**无 P1 兜底**(独立产物),故需覆盖其部署面。

---

## 1. 决策:Path C —— 一个 `relon-IR → LLVM-IR` 发射器,两个 LLVM 目标

不写两个发射器。写**一个** `relon-IR → LLVM-IR`,再让 LLVM 出 **native(P2)** 或 **wasm32(P3)**。

**Spike 证据(2026-06-03,已验证)**:系统 LLVM 18.1.3 含 WebAssembly target;inkwell 0.9(加 `target-webassembly` feature)实测从 LLVM-IR 发出 **154 字节 wasm32 object,magic `\0asm`**;rustc 有 `wasm32-*`(host→wasm bitcode)。

**收益**:
- **relooper(CFG→结构化 wasm)由 LLVM 自己做**——不手写。
- **优化器 + host co-compile(LTO 跨内联)两目标白送**。
- 可**退役手写的 `relon-codegen-wasm`(Phase Z POC)**——P3 改由 LLVM 出 wasm。
- codegen 实现从「cranelift + llvm + 手写 wasm」收成「**cranelift(P1 运行时)+ llvm(P2/P3 共用)**」。

**代价**:wasm 路也吃 LLVM 工具链(构建期;两条线本就闭世界 build-time co-compile,可接受)。若将来需要「运行时、无 LLVM 的轻量 wasm 生成」,再保留一个轻量 encoder——目前不需要。

---

## 2. 架构

```
源码 ─(共享前端)─► relon-IR ──► [ 一个 relon→LLVM 发射器 ] ──► LLVM-IR ──┬─► native object  (P2: relon-rs 原生二进制)
                       ▲                                                  └─► wasm32 object  (P3: wasmtime/浏览器)
                       │
                cranelift(P1)= 语义 oracle:每个 Op「该算什么」的活规范 + 差分对齐标尺
```

- **共享前端**:`relon_ir::frontend::compile`(已 done)。
- **内存模型已可移植**:relon 的 buffer 协议本就用 **i32-arena 偏移**(cranelift 的 wasm-AOT 血统)→ native 堆 / wasm 线性内存两边都贴。
- **co-compile 脊梁**(host 一体编译):宿主 Rust `--emit=llvm-bc` → 与 Relon 的 LLVM-IR **LTO 合一单元** → `CallNative`/装饰器/闭包回调 = **单元内直调,可 inline**(不是 FFI/host-import 边界——那是 P1 开世界的活)。
- **target 差异**(就两处真分歧):① 指针宽度(native 64 / wasm32 32)→ 发射用 DataLayout 抽象;② **effectful host fn**:native 直接编进;wasm 编不进 → 退化成 **WASI/host import**(纯计算 host fn 两边都内联)。

---

## 3. 模块结构(为并行 + 穷尽检查)

`relon-codegen-llvm` 当前是单文件 `emitter.rs`(6152 行)+ 一个大 match——**不能多人并行改**。重构成**照 cranelift `codegen/` 的 per-family 布局**:

```
src/codegen/
  mod.rs          # OpVisitor 分发(穷尽,新增 Op 编译期报错)+ 共享 Codegen 状态
  arith.rs        # Add/Sub/Mul/Div/Mod/BitAnd/cmp        (Phase B 已有,做参考样板)
  control.rs      # If/Loop/Block/Br/BrTable/Select        (Phase B 已有)
  mem.rs          # LoadField/StoreField/Load*/Store*AtAbsolute/arena
  collections.rs  # ConstList*/ListGet/DictGet/AllocRecord/PushRecordBase…
  string.rs       # Add(String)/StrConcat(N)/ConstString/StrLen…
  closure.rs      # MakeClosure/CallClosure(闭包表)
  call.rs         # Call(stdlib inline)/CallNative/CheckCap
  schema.rs       # schema 方法 dispatch
  unicode.rs      # *TableAddr / case-fold / 组合标记等长尾
```

每个 family 一个文件 → 并行 agent 改各自文件、互不冲突;`mod.rs` 的分发用 OpVisitor(每臂瘦委托),家族未实现处统一 `unsupported`(可编译、运行时报错)。

---

## 4. 分阶段(每阶段:对 cranelift 差分对齐、全绿可回退、worktree 推进)

| 阶段 | 内容 | 并行性 |
|---|---|---|
| **Spike** ✅ | inkwell→wasm32 object 可行(已验证) | — |
| **0a 骨架(前置)** ✅ | **行为不变的重构**:emitter.rs → per-family 模块 + 瘦分发 seam | **串行**(已完成,见 §7) |
| **0b family 填充** ✅ | 逐 family 把 Op 降到 LLVM-IR(照 cranelift 移植)+ 差分:control · mem · schema · collections · call · unicode | **并行**(已完成,见 §7) |
| **1 P2 完整** | co-compile LTO 脊梁(host bitcode + 合一 + CallNative 直调)+ Phase C cap/sandbox(信任前置)+ relon-rs 集成 | 串行为主 |
| **2 P3 翻 target** | target=wasm32:DataLayout 指针宽度 + effectful-host→WASI + wasmtime 运行 + 接 `Backend` 枚举;**退役手写 relon-codegen-wasm** | 串行 |
| **3 性能峰** | LTO 跨内联深化(打平/超手写) | — |

**差分对齐方法**:用 test-harness 的 three-way(tree-walk / cranelift / llvm)对每个 family、每个覆盖到的 workload 比对结果,cranelift 是金标准。

---

## 5. 风险 / 注意

- **0a 是并行前置**:不先拆模块就并行 = 同一 match 冲突(等同删-crate 的紧依赖陷阱)。
- **指针宽度**:0a 起就用 DataLayout 抽象指针/GEP,别硬编 64,免得 P3 翻 wasm32 时返工。
- **effectful host fn 过 WASI**:P3 唯一硬分歧,P2 不涉及;留到 Phase 2 处理。
- **co-compile LTO**:P2 的 GraalVM 级脊梁,Phase 1 才上;0a/0b 先把「单 emitter 全覆盖」做出来。
- **维护账**:完成后 codegen 线 = cranelift + llvm(两目标),新增 Op 要两处实现——已被「部署价值(native + 浏览器)」付账,且穷尽检查保漏写即编译失败。

---

## 6. 起点
**Phase 0a(骨架重构,串行)** 先行 → 落地后 **Phase 0b 按 family 并行 fan-out**。loop 驱动 + 集成 + 排序。

---

## 7. Phase 0 实施结果(2026-06-03,已完成、全绿)

**0a 骨架**:`emitter.rs`(6152 行)行为不变地拆成 `codegen/{mod,arith,control,mem,collections,string,closure,call,schema,unicode}.rs`;另加 **0a.1 seam**:把共享的 fat unsupported 大臂拆成 6 条 per-family 瘦委托(`lower_<family>_rest`),令 0b 各 family agent 只改自己的文件、零 mod.rs 冲突。

**0b 并行填充(6 worktree agent,各照 cranelift 移植 + 差分)**:

| family | 新增覆盖 | 仍 unsupported | 差分对齐 |
|---|---|---|---|
| **control** | `Select`(build_select)`BrTable`(build_switch) | — | cranelift-gold + tree-walk(经 min/max),全绿 |
| **mem** | `LoadFieldAtAbsolute` | — | cranelift **本身不支持**此 op(无 oracle);见下「封套」 |
| **schema** | `LoadSchemaPtr` | schema-method dispatch(暂无源级路径) | tree-walk 为值 oracle(cranelift 不支持) |
| **collections** | `AllocSubRecord` `PushRecordBase` `EmitTailRecordFromAbsoluteAddr` | `ConstListInt/Float/Bool`(需共享 ConstPool 加 list_*_offsets)`ConstListString`/`ListGetByIntIdx`/`DictGetByStringKey`(**cranelift 亦不支持**) | IR 形状 + cranelift codegen parity + 手验 IR |
| **call** | `CheckCap`(i64 caps 位掩码门)`Trap`(llvm.trap) | `CallNative`(需 evaluator 侧 host-fn registry + MCJIT 符号接线 + Emit 接 imports) | CheckCap/Trap 语义锚在 cranelift 运行时;LLVM 侧编译正确性 + 非法入口拒绝 |
| **unicode** | 全 10 个 `*TableAddr`(私有去重 global + scratch memcpy,字节与 cranelift 编码器一致) | — | 表字节 byte-for-byte == relon_ir 编码器 + 去重/长度 |

**跨切发现(均非 family codegen bug,留待 Phase 1)**:
1. **宿主 arg/return 编组封套(Phase B 限制,共享 `evaluator.rs`)**:`write_value_into_builder` 仅有 Int/Float/Bool/Null 标量臂,**无 Schema 参/List 参/嵌套返回**支持。故 schema-field 全路径 **codegen 已通(from_source 成功降级 LoadSchemaPtr+LoadFieldAtAbsolute),但 run_main 端到端跑不通**——卡在编组,而非任一 op。已用诚实边界测试钉住(`phase0b_{mem,schema}` 断言:build 成功 + run 止于编组封套 + 无 unsupported-op),Phase 1 接通编组后升级为值断言。
2. **cranelift 不是部分 op 的 oracle**:`LoadFieldAtAbsolute`/`LoadSchemaPtr`/`ListGetByIntIdx`/`DictGetByStringKey`/`ConstListString` 在 cranelift 也是 unsupported;这些 op 的差分以 tree-walk 为金标准(可达时)或锚在 IR/编码器层。
3. **`CallNative` 是 Phase 1 的核心串行项**:需 ① evaluator 侧 Arc host-fn registry + state 接线 ② MCJIT 注册 `relon_call_native`/`cap_lookup` 符号 ③ `Emit` 接 `imports`/`ir` 句柄。lowering 体本身可移植进 call.rs,但三项共享接线必须先行(详见 call family 报告)。

**留给 Phase 1 的 LLVM 覆盖缺口**:`CallNative`、`ConstList*`、宿主 Schema/List 编组。其余(标量算术、控制流、闭包、字符串、记录构造、能力门 CheckCap/Trap、schema 指针、字段/绝对寻址、全 Unicode 表)已覆盖。

---

## 8. Phase 0b 收口(2026-06-03,已完成、全绿)

§7 三个缺口已收口(Wave 1 并行 + CallNative 串行,均差分锚 cranelift/tree-walk):

| 缺口 | 结果 | 关键 |
|---|---|---|
| **宿主 Schema 编组** | ✅ 端到端打通 | `write_value_into_builder` 加 Schema 臂(递归 `sub_record`/`finish_sub_record` 回填 `LoadSchemaPtr` 读的偏移槽)+ `read_value_from_reader` 嵌套 Schema 返回解码。schema-field **真出 42/7**,边界测试升级为真实值断言(tree-walk 金标准)。buffer 协议本就支持子记录,无跨 crate 改动。 |
| **`ConstListInt/Float/Bool`** | ✅ 降级完成 | ConstPool 加 `list_*_offsets` + 字节布局(int/float align8 `[len][pad][i64/f64…]`、bool align4 `[len][u8…]`),与 cranelift byte-for-byte 一致。ConstListInt 经 from_source codegen parity 验证;Float/Bool 字段被前端拒(不可达),以字节级单测钉死。 |
| **`CallNative`** | ✅ 开世界动态分发打通 | 镜像 cranelift `emit_call_native_dynamic`/`RelonCallNative`:evaluator 挂 `HostFnRegistry`,`relon_llvm_call_native` helper 经 MCJIT global mapping 接入,Emit 接 `imports`/state,buffer entry 贯穿 caps + `trap_code`。源码降级的 CallNative(`NO_CAPABILITY_BIT`,由前置 CheckCap 把守)**端到端可执行**;grant/deny/dispatch 三态对齐 cranelift。Trap 改写 `trap_code` sentinel(`llvm.trap` 是不可捕获的 SIGILL)。 |

**仍 unsupported(已知边界,非本轮目标)**:
- `ListGetByIntIdx` / `DictGetByStringKey` / `ConstListString` —— **cranelift 亦不支持**(无 oracle,属未设计区)。
- `CallNative` 能力门**直连路径**(`cap_bit != NO_CAPABILITY_BIT` 的 cranelift legacy `cap_lookup` + `call_indirect`)—— 源码降级从不走此路(能力由前置 `Op::CheckCap` 独立把守),诚实留 unsupported。
- **List 类型的返回值解码**(`read_value_from_reader` 的 List 臂)—— Phase B return 解码器限制,留 Phase 1。
- schema-method dispatch —— 暂无源级路径触发。

**结论**:LLVM-AOT 后端现已覆盖**源码可达的全部主流 Op 面**(标量/控制流/闭包/字符串/记录/能力门/native 调用/schema 字段/const list/全 Unicode 表),三态能力安全 + native 分发端到端验证。余下 unsupported 均为 cranelift 同样未实现或 Phase-B/Phase-1 封套项,已诚实记录。
