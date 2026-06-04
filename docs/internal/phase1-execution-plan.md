# Phase 1(P2 原生 AOT 完整)+ Phase 2(P3 wasm):并行施工 DAG

- **状态**:开工中(2026-06-03)。Phase 0/0b 已完成(见 [`p2-p3-completion-plan.md`](./p2-p3-completion-plan.md) §7-§8)。
- **本文档 = cron loop `e8c434a4` 的真相源**:lane 文件归属、stage 依赖序、green gate 以此为准。
- **关联**:[`adr-execution-tiers.md`](./adr-execution-tiers.md)(co-compile/闭世界 vs 开世界)。

---

## 0. 探明的地基(决定 DAG 形状)

1. **原生 AOT 路径结构上已 ~95% 完整**:`emit_object`(evaluator.rs:1775)已产 buffer-protocol ELF .o,经 `emit_module_funcs` 贯穿 `imports`、降级 user helper/closure。窄口不在发射器。
2. **`lower_field_descriptors`(evaluator.rs:2013)是原生签名面的唯一卡点**:match 只收 Int/Bool/Null/String;Float/List*/Schema 全报 `UnsupportedSignature`。
3. **编组封套是必须同步的三元组**:`EmittedFieldType` 在三处必须 byte-for-byte 一致 —— `evaluator.rs`(codegen-llvm)、`relon-rs-shims/src/marshal.rs:73`(ArgValue/RetValue/call_buffer_entry)、`relon-rs-build/src/lib.rs:480-501`(render_one_buffer Rust 类型映射)。**拓宽一个类型 tag = 三文件协调改 = Phase 1 的中心串行化风险**。
4. **MCJIT 侧已能编组 Float/Schema**(write_value_into_builder/read_value_from_reader),但 AOT 绑定侧 `lower_field_descriptors` 拒 Float —— 此不对称正是 lane ① 要消除的。
5. **CallNative 开世界分发已全通**(`relon_llvm_call_native` via add_global_mapping evaluator.rs:707)。**LTO 脊梁是其旁的新增闭世界路径,非重写**;开世界路径留作 MCJIT/fallback。
6. **指针宽度已抽象**(mem.rs:51-54,i32 arena 偏移 + zext-i64 + i8* GEP)。P3 唯一真分支在 `TargetMachine` 构造(evaluator.rs:1938)+ `relon_llvm_call_native` 的指针 FFI 签名。
7. **`Backend` 枚举(relon/src/lib.rs:596)无 WasmAot 变体**;P3 退役的 wasm crate 共 ~10.6k 行。

---

## 1. Stage DAG(依赖序)

```
Stage 1(serial-foundational seam)      Stage 2(并行 fan-out)          Stage 3(P3,gates on all S2 绿)
  S1.A marshal-widening seam ──┐
   (3 文件 EmittedFieldType 三元组拆 per-variant 助手)
                                ├─► S2.① Float 编组 ─┐
  S1.B LTO co-compile 脊梁 ──┐ │   S2.② List 编组   │
   (闭世界 CallNative 直调)   │ │   S2.③ Phase C cap/sandbox(可即起,全新文件)
                              │ │   S2.④ relon-rs 集成 ├─► S3.X wasm32 retarget
  S1.A ⊥ S1.B(文件不相交,    │ └─► S2.⑤ LTO inline 深化(待 S1.B)│   (native 是 wasm 差分 oracle)
   首发 wave 同时起)          └────────────────────────────────────┴─► S3.Y 退役 codegen-wasm(最后)
```

- **S1.A 必须先于 ①②④**(都碰 EmittedFieldType 三元组);拆 per-variant 助手后各 lane 拥各自变体,类比 0b 的 0a.1 seam。
- **S1.B 与 S1.A 正交**(文件不相交),首发 wave 并发;它先于 S2.⑤。
- **Stage 3 gate on 全部 Stage 2 绿**:原生 AOT 是 wasm 差分金标准。

---

## 2. Lane 文件归属(零重叠证明)

### Stage 1 — serial seam(各一 agent,先于 Stage 2 并入)

**S1.A marshal-widening seam**(行为不变重构):
- `crates/relon-codegen-llvm/src/evaluator.rs`:`lower_field_descriptors`(2013-2051)、`EmittedFieldType` 枚举、`write_value_into_builder`/`read_value_from_reader`(1401/1485)的 per-type 臂拆成 `marshal_<type>` 助手(每类型一 fn,镜像 0a.1 的 `lower_<family>_rest`)。
- `crates/relon-rs-shims/src/marshal.rs`:`call_buffer_entry`(235-410)pack/unpack match 拆 per-variant 助手;三枚举加「新增变体 = 加变体 + 3 个兄弟助手」契约 + 跨三 crate round-trip 穷尽测试。
- `crates/relon-rs-build/src/lib.rs`:`render_one_buffer` 类型映射(480-501)拆成 `rust_type_for(EmittedFieldType)` 表 fn。
- Green gate:`phase0b_*` + `relon-rs-build/tests/integration.rs` 维持绿。

**S1.B LTO co-compile 脊梁**(与 S1.A 文件不相交):
- `crates/relon-codegen-llvm/src/codegen/call.rs`:在开世界 `relon_llvm_call_native` emit 旁加 `emit_call_native_direct`——host fn 编译期已知(build.rs 路径)时发 `call @<host_symbol>` 到 external 声明,而非动态 helper。参照 cranelift `call.rs:262` 的 *static* 臂(cap_lookup→fn_ptr 直连,**非** :337 的 _dynamic 臂)。
- `crates/relon-codegen-llvm/src/codegen/mod.rs`:`emit_module_funcs`(372)加 closed/open-world 标志贯穿 `Emit`。
- **NEW** `crates/relon-codegen-llvm/src/cocompile.rs`:`rustc --emit=llvm-bc` host shim crate → `link_in_module` 与 Relon 模块合一 → LTO/internalize 使 co-resident host fn inline(复用 evaluator.rs:2125 `run_default_o3_pipeline`)。
- **先 spike**:inkwell 0.9 `Module::link_in_module` + rustc-LLVM/system-LLVM-18 bitcode 版本兼容性(见风险 1),不通则报告不硬上。
- Green gate:新 `tests/cocompile_inline.rs` 断言链接后模块对已知 host fn 零 `call @relon_llvm_call_native`(已 inline)且值 == 开世界路径;差分锚 cranelift `native_call_from_source.rs`。

### Stage 2 — 并行 lane(S1.A/S1.B 并入后文件不相交)

| lane | 拥有文件 | 产物 | 参照 | green gate |
|---|---|---|---|---|
| **S2.① Float** | evaluator.rs 的 Float 臂 + `marshal_float`;marshal.rs 的 `*::Float` + 3 助手;rs-build 的 `Float=>("f64"…)` 表行 | `#main(Float)->Float` 原生二进制 | MCJIT 侧已编组 Float | 三方 vs tree-walk + 现有 `llvm_w28_float_mixed.rs`/`llvm_f64_arith.rs` 经 emit_object 重跑 |
| **S2.② List** | evaluator.rs 的 List* 臂 + `marshal_list`;marshal.rs 的 `*::ListInt…` + layout;rs-build 的 `&[i64]`/`Vec<i64>` 表行 | `#main(List)->List`(先 ListInt) | ConstPool list layout(mod.rs add_list_int)+ cranelift `w16_list_materialize_aot.rs` | tree-walk 金标准(cranelift 不支持 ListGet)+ 字节层 + 新 `tests/aot_list_roundtrip.rs` |
| **S2.③ Phase C cap/sandbox**(可即起) | **NEW** `crates/relon-codegen-llvm/src/sandbox.rs` + **NEW** `src/vtable.rs`;**只读消费** state.rs(勿改,S1 已冻) | 能力门在**原生**(emit_object)入口生效,非仅 MCJIT | 直接移植 `relon-codegen-cranelift/src/sandbox.rs`(1201 行)+ `vtable.rs` | cranelift `host_fn_capability.rs`/`vtable_indirection.rs`/`trap_div_zero.rs` 金标准;新 `tests/aot_cap_gate.rs` 在**链接后二进制**上验 grant/deny/dispatch 三态 |
| **S2.④ relon-rs 集成** | rs-build `emit_all`(185-304);`relon-rs-shims/src/lib.rs` + `sandbox_state.rs`;`relon-rs-demo/*`;`relon-rs-macro/src/lib.rs` | 更新「Phase 1 envelope」文档 + 收宽签名集;demo 演示 Float/List/cap | 现有 `integration.rs` | integration.rs 扩 + demo build/run 绿 |
| **S2.⑤ LTO inline 深化**(待 S1.B) | `cocompile.rs`(S1.B 交接);evaluator.rs `run_default_o3_pipeline`(2125)调优 | host↔guest 跨 inline,CallNative 开销→零 | Rust LTO / GraalVM 偏特化(ADR §2.1) | perf bench;断言 inline 后调用数 |

**碰撞处理**:①②④ 都碰 `rs-build/lib.rs` 与 `marshal.rs`。S1.A 拆成 per-variant 助手表后,① 拥 Float 行、② 拥 List 行、④ 拥 emit_all/doc —— 行区不相交。**若 loop 驱动者无法保证同文件行区不相交,则 Stage 2 内 ①→②→④ 串行**(都小),③⑤ 真并行(③ 全新文件、⑤ 拥 cocompile.rs)。这是最安全调度。

### Stage 3 — P3 wasm(串行,gate on 全部 Stage 2 绿)

**S3.X wasm32 retarget**:
- `evaluator.rs` `emit_object`(1932-1968):`TargetMachine` 构造按 `Target` 枚举(Native vs Wasm32)参数化;加 `initialize_webassembly`;建 `wasm32-wasi`/`wasm32-unknown` triple + DataLayout 替 `get_default_triple`+host CPU。mem.rs 已发 i32-offset GEP,body 不需改。
- **NEW** `crates/relon-codegen-llvm/src/wasi_host.rs`:effectful host fn → WASI import(P3 唯一分歧,ADR §2.2);纯 host fn 仍经 S1.B cocompile inline。
- `crates/relon/src/lib.rs`:加 `Backend::WasmAot` 变体(596)+ `BackendError::WasmAot`。
- `crates/relon-cli/src/main.rs`:`--backend wasm` + wasmtime run。
- Green gate:把旧 `relon-wasm-evaluator/tests/w*_smoke.rs` 语料经新 LLVM→wasm 路径在 wasmtime 重跑,差分 vs native(此时是 oracle)。

**S3.Y 退役手写 wasm**(**最后**,仅在 S3.X 过全部旧语料后):删 `crates/relon-codegen-wasm`、`relon-wasm-evaluator`;审 `relon-wasm-bindings`。拥这些 crate 目录 + workspace Cargo.toml 成员。

---

## 3. 风险

1. **LTO 工具链(S1.B,最高风险)**:host bitcode 来自 rustc 自带 LLVM,Relon 模块来自 system LLVM 18.1.3(inkwell llvm18-1)。**bitcode 版本 skew 会断 `link_in_module`**。缓解:把 rustc 钉到 LLVM major==18 的工具链,或用同一 llvm-sys 出 host bitcode。**先 spike 验证**再投 S2.⑤。
2. **开世界→闭世界 CallNative 测试影响**:S1.B 加*并行*直调路径,**勿删**动态路径(MCJIT/from_source 仍需,evaluator.rs:707)。风险:本该闭世界的 build.rs 源误走动态 helper(值对但没 inline)→ `cocompile_inline.rs` 断言 inline 数而非仅值。
3. **指针宽度返工(S3.X)**:低风险(mem.rs 已抽象)。唯一未审点:`relon_llvm_call_native` 的 `ctx.ptr_type` FFI 签名(mod.rs:872)——wasm32 下是 32 位指针,但 helper 本身是 native Rust fn,wasm 下须变 WASI import 或被 inline(S3.X/S1.B 处理)。
4. **codegen-wasm 退役时机**:严格**最后**。旧 `relon-wasm-evaluator` 测试是 S3.X 的验收语料,过 parity 前删 = 丢 oracle。两边并存到新路径全绿,再一次性删。
5. **EmittedFieldType 三元组漂移**:S1.A seam 没干净落地 → ①②④ 产三个不一致枚举(codegen 发的 tag shim 解不了 → call_buffer_entry 静默 UB/panic)。缓解:S1.A 加编译期穷尽断言 + 跨三 crate round-trip 测试。

---

## 4. 首发 wave(立即派)

三个并发、文件零共享:

| lane | 拥有 | 首交付 |
|---|---|---|
| **W1-A = S1.A marshal seam** | evaluator.rs(lower_field_descriptors + EmittedFieldType + write/read per-type 拆)、relon-rs-shims/marshal.rs(拆 call_buffer_entry)、rs-build/lib.rs(render_one_buffer→rust_type_for 表) | 行为不变 per-variant seam;phase0b_* + integration.rs 绿 |
| **W1-B = S1.B LTO 脊梁** | codegen/call.rs(emit_call_native_direct)、codegen/mod.rs(closed/open 标志)、NEW cocompile.rs、NEW tests/cocompile_inline.rs | `--emit=llvm-bc` + link_in_module spike 绿;一 host fn inline、值 == 开世界 |
| **W1-C = S2.③ cap/sandbox**(即起) | NEW codegen-llvm/src/sandbox.rs + src/vtable.rs;只读 state.rs | 移植 cranelift sandbox/vtable;链接后二进制验能力三态 |

三者文件零共享(evaluator/marshal/rs-build vs call/mod/cocompile vs 全新 sandbox/vtable)。①②④ 等 W1-A;⑤ 等 W1-B;③ 即 W1-C。

---

## 5. Phase 1(P2)实施结果(2026-06-03,已完成、全绿)

Stage 1 + Stage 2 全部 lane 已并入 main、`cargo build/test/clippy --workspace` 全绿、worktree 全清。

| lane | 结果 | 关键证据 |
|---|---|---|
| **S1.A** marshal seam | ✅ | `EmittedFieldType` 三元组拆 per-variant 助手 + 编译期穷尽 round-trip 护栏(防三 crate 漂移) |
| **S1.B** LTO co-compile 脊梁 | ✅ | 闭世界 CallNative 直调;**bitcode skew(rustc-LLVM22 vs 系统 LLVM18)经 `--emit=llvm-ir`+`llvm-as-18` 文本桥解**;post-O3 host fn 完全 inline |
| **S2.①** Float | ✅ | 原生 `.o` 描述符 + 值层 bit-identical 三方(tree-walk/cranelift/llvm) |
| **S2.②** ListInt | ✅ | **List 返回解码缺口关闭**(const + 参数派生),三方对齐 |
| **S2.③** Phase C sandbox/vtable | ✅ | sandbox.rs/vtable.rs 移植;能力门 IR 烤进原生对象;object e2e 经 S2.⑤ 的 emit_object options 解锁 |
| **S2.④** relon-rs 集成 | ✅ | **真·原生 link-and-run demo**:Float→11.5、Int→List=[10,11,7] 等 6 形态全对(真 .o 链接 + typed 调用 + 正确解码值) |
| **S2.⑤** 源码驱动闭世界 buffer + emit_object options | ✅ | `from_source_closed_world` JIT buffer entry:post-O3 **零 `relon_llvm_call_native`、零 `call @<host>`**、inline 留存、值 == 开世界 == cranelift 锚;`emit_object_with_options` 解析 `#native`(旧签名保留) |

**P2 现状达成**:
- **原生签名面**:Int/Float/Bool/Null/String/Schema(参)/ListInt —— 远超原「仅 Int」封套。
- **co-compile LTO 脊梁(marquee)**:CallNative 单元内直调,源码驱动的 buffer 程序 host fn post-O3 完全 inline,值正确锚 cranelift。GraalVM 式闭世界**已打通**。
- **Phase C 能力门**:`Op::CheckCap` 烤进原生对象,sandbox/vtable 移植到位,object emit e2e 解锁。

**剩余 unsupported / 诚实缺口(留作已知边界)**:
- **ListFloat/ListBool**:relon-ir 前端拒这些字段(不可达,非后端缺口)。
- **List 参→List 返值路径**:frozen JIT codegen 不跨 arena 拷贝记录(仅描述符层验证)。
- **relon-rs crate 内的 cap/`#native` e2e**:codegen-llvm 的 object 路径现已能 emit+解析 `#native`(S2.⑤),但 relon-rs 运行时 shim 层 `call_buffer_entry` 硬编 caps=0 + 无 `#native` 分发表 —— 从链接后二进制调带门 `#native` 尚未接(relon-rs-shims/marshal 残留)。
- **LTO bitcode 桥**:现用 `llvm-as-18` 文本桥(版本 skew),前向兼容是经验性;长期稳妥 = 用同 llvm-sys 出 host bitcode(W1-B 标注,deferred)。
- **ListGet/DictGet/ConstListString/schema-method dispatch**:沿 0b,cranelift 亦不支持或无源级路径。
- **Stage 3 perf**:S2.⑤ 已做基础 inline 深化;独立的 LTO inline 深度调优未单独追。

---

## 6. P2 收口(2026-06-03,已完成、全绿、已推送)

§5 列的三个残留缺口已收口(三 lane 并行,文件不相交;首轮撞 session limit、重置后从干净 main 重跑):

| 缺口 | 结果 | 关键 |
|---|---|---|
| **G1 cap/`#native` e2e** | ✅ **链接后原生二进制端到端** | `Compiler::source_with_native_fns` + `NativeHostFn`:门控源闭世界编译、host 体内联进 `.o`;`SandboxState` 带授权 caps 位掩码(原硬编 0)作 buffer entry 第 6 个 `i64 caps` 实参;deny 经 `trap_code` 升类型化 `CapabilityDenied`(无 SIGILL)。**demo 实证两态**:`secret::main(grant,42)=Ok(1700000042)` / `(deny,42)=Err(CapabilityDenied)` |
| **G2 List 参→返跨 arena 拷贝** | ✅ 修复(根因纯 codegen-llvm,非 IR) | mem.rs 两缺陷:① 指针-indirect 参 load 推的是 input-buffer 相对偏移而下游按 arena 相对(补 `+in_ptr`)② 单次 memcpy 拖错源记录 `[len][pad]` 几何到 8-对齐输出槽(payload 错位 4 字节)→ 头与 payload 分别拷、各自 `align_up`。`xs->xs` 多用例三方对齐 tree-walk |
| **G3 去 llvm-as-18 外部依赖** | ✅ 进程内 inkwell 解析 | `rustc --emit=llvm-ir` → `MemoryBuffer::create_from_file` + `Context::create_module_from_ir`(免外部 `llvm-as-18` 二进制);cocompile_inline/buffer 仍全绿(等价) |

**P2 现状(收口后)**:原生签名面 Int/Float/Bool/Null/String/Schema(参)/ListInt(含 List 参→返);**co-compile LTO 闭世界 + 能力门 `#native` 在链接后原生二进制端到端**(grant/deny 两态实证);LTO 桥无外部二进制依赖。

**收口后仍存的已知边界**:
- `String s -> s` 恒等被另一处 String 参编组 seam 挡(G2 发现的既有小 seam,非 codegen 拷贝问题)。
- ListFloat/Bool(前端拒)、ListGet/DictGet/ConstListString/schema-method dispatch(cranelift 亦不支持)—— 沿 0b。
- LTO 文本 IR 前向兼容仍是经验性(外部二进制依赖已去);长期可考虑同 llvm-sys 出 bitcode。

---

## 7. P3(wasm)实施结果(2026-06-04,核心+parity+WASI 完成、全绿、已推送)

| lane | 结果 | 关键 |
|---|---|---|
| **S3.X wasm32 retarget** | ✅ | `CodegenTarget{Native,Wasm32}`;`emit_object_for_target` 参数化 TargetMachine(wasm32-wasi triple、DataLayout `p:32:32` 从 get_target_data)。LLVM→wasm32 object(`\0asm`)→ `wasm-ld`(`wasm_link.rs`)→ wasmtime,9 workload 值对齐 native oracle。arena 体未改(width-agnostic)。`emit_object` 仍是 Native thin wrapper(字节等价) |
| **parity 覆盖矩阵** | ✅ | 旧 WasmEvaluator 全语料过新路径:13 FastInt(w1/2/5/6/8/9/10/12 等)+ String/const-string/List<Int>/多字段 Dict buffer 返回**全对齐**;3 个发射 gap 诚实标 ❌(见下) |
| **WASI effectful host** | ✅ | `wasi_host::emit_call_native_wasi`:effectful `#native` 在 wasm32 降成 `(import env <name>)`,wasmtime host 提供。CallNative 分流:ClosedWorld→inline(native)/ Wasm32→import / OpenWorld-native→`relon_llvm_call_native`。`clock_add(35)=42` 经 import 跑出(hit-counter==1 证跨边界),对齐 native |

**P3 现状**:同一 `relon-IR→LLVM-IR` 发射器经 target 切换出 native **或** wasm32;wasm 经 wasm-ld→wasmtime 跑、值对齐 native(native 是 oracle);effectful host 过 WASI import。**已退役 trace-JIT/bytecode 后,codegen 线收成 cranelift(P1 运行时)+ llvm(P2 native / P3 wasm 共用一个发射器)**。

**3 个 wasm32 发射 gap(诚实,parity 测试钉为 unsupported)**:w4 `.contains` filter(wasm32 ConstString const-pool 布局)· w5 production 嵌套 Dict 字段(anon-dict-return 不收嵌套)· w7 递归闭包(`MakeClosure` —— object-emit native+wasm **共有**预存限制,非 wasm 专属)。

**退役决策(S3.Y,待用户拍板,本 loop 不擅删)**:
- `relon-codegen-wasm` + `relon-wasm-evaluator`:**接近可退役**(新路径覆盖 FastInt 全语料 + String/List/Dict 返回);差 w4/w5 两真实 gap(w7 共有)。补齐这俩即 100% parity,或接受这 3 workload 不经 AOT-wasm、由 parity 测试 + native 接管 oracle。
- `relon-wasm-bindings`:**不可退役** —— 浏览器解释器(tree-walk via wasm-bindgen)+ LSP(format/hover/goto/complete/rename/inlay),与 AOT codegen **正交**,新路径无替代、也不打算替代。

**Backend::WasmAot**:不加回(`relon/src/lib.rs` 已主动退役该变体)。wasm 是**构建期 emit 产物**(像 native .o),非进程内运行时 backend —— 与架构一致。

---

## 8. Follow-up:wasm 线副作用对齐「标准 wasm / 标准 WASI」(待 co-compile lane 完成后做)

**北极星**:relon 的 wasm 产物就是一个**标准、可移植、生态原生的 wasm 模块**;处理副作用/外部依赖**照常规 wasm 程序那套走**(effectful → import,宿主提供),不自造 relon 专属隔离。

**现状**:`wasi_host.rs` 把 effectful `#native` 降成 **`(import "env" "<name>")`** 自定义 host import —— 由 relon 自己的 wasmtime runner 经 `Linker::func_wrap` 提供。**精神对(effectful→import)但绑死 relon runner**:这个模块只能在 relon 的 runner 里跑。

**目标(follow-up)**:把 effectful 能力映到**标准 WASI 接口**而非 `env::*` 自定义 import:
- `reads_clock` → `wasi:clocks/wall-clock`(preview2)/ `clock_time_get`(preview1)
- 文件 → preopened dir / `fd_*`;网络 → `wasi:sockets`;随机 → `wasi:random`;env/args → `environ_get`/`args_get`
- → relon 编出的 wasm 跑在**任何 WASI host**(wasmtime / wasmer / 浏览器 jco / 云 wasm 平台),不绑 relon runner。

**契合点**:WASI(尤其 preview2 / component model)**本身 capability-secure**(无 ambient 权限,时钟/目录/socket 显式授予),与 relon 的 `CheckCap` / `CapabilityBit` / `NativeFnGate`(`requires <cap>`)**天生同构**。`requires reads_clock` ↔ 宿主授予 `wasi:clocks`,两层能力检查叠在同一条 import 边界上。

**正交说明**:本 follow-up 只关 effectful import 的**形状**(自定义 `env::*` → 标准 WASI);与正在做的**纯计算宿主 co-compile inline**(把 pure `#native` inline 进 wasm 单元、无 import)互不影响 —— effectful 不论怎样都走 import,只是 import 改成标准 WASI 形态。

**待办时机**:co-compile lane(纯计算宿主一体编译进 wasm)落地后再开本 follow-up lane。
