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
| **0a 骨架(前置)** | **行为不变的重构**:emitter.rs → per-family 模块 + OpVisitor 分发;native MCJIT 路径照旧;arith/control 作样板;其余 family `unsupported` 占位 | **串行**(必须先于 0b) |
| **0b family 填充** | 逐 family 把 Op 降到 LLVM-IR(照 cranelift 对应文件移植)+ 对 cranelift 差分对齐:mem · collections · string · closure · call(含 CallNative/CheckCap)· schema · unicode | **并行**(各改各文件) |
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
