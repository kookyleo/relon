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
