# ADR-E：WASM runtime 选择（2026-05-16）

> Phase 0 子项 8/8（最后一份）。
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> §二 待定子问题 E "Wasm runtime 工具选择"。

## Context

Phase 8 把 `WasmAotEvaluator` 集成进 `relon` facade 后，host runtime
需要一个 wasm engine 跑生成的 `.wasm` bytes。市面上至少 4 个选项：

| Runtime | 形态 | 主要应用 |
| --- | --- | --- |
| **wasmtime** | Rust crate / CLI | server-side, embedded, edge |
| **wasmer** | Rust crate / CLI | server-side, plugin systems |
| **wasm3** | C library (Rust bindings) | embedded / tiny footprint |
| **浏览器 V8 / SpiderMonkey** | JS API | playground / web |

## Decision

facade **不绑定具体 runtime**——抽出 `trait WasmRuntime` 适配层；
缺省提供 **wasmtime** 实现（server / 后台 / 嵌入服务通用）；浏览器
端走 JS 原生 wasm API（已有的 `relon-wasm` cdylib 路径）。

## Rationale

### 1. 不绑定的成本几乎为零

`WasmAotEvaluator` 跟 runtime 的接触面其实**很小**：

- instantiate module
- call exported `run_main(in_ptr, in_len, out_ptr) -> i32`
- read linear memory（in_buf / out_buf）
- catch traps with bytecode offset
- link host fn imports

这五个 op 在 wasmtime / wasmer / wasm3 / browser 都有等价 API。
抽 trait 100-200 行代码——比"承诺只支持 wasmtime"灵活得多，未来
换 runtime 不需要改 evaluator。

### 2. 不同部署场景需要不同 runtime

- **后台服务 / CLI**：wasmtime（功能全、稳定、Bytecode Alliance
  亲生）
- **浏览器 playground**：JS 原生 wasm（已有的 `relon-wasm`，零额外
  依赖）
- **嵌入式 IoT**：wasm3（KB 级体积）
- **多租户 PaaS**：wasmer（fast cold start、edge 友好）

把 runtime 选择交给 host，让 Relon 适应这些场景而不是 lock-in 单一
后端。

### 3. wasmtime 作为默认是社区共识

- Bytecode Alliance 官方
- Rust 项目默认（cargo install wasmtime 即用）
- 文档 / 工具最完备
- 跟 `wasm-encoder` / `wasmparser` 同一组 crate 维护，版本兼容性好

如果 host 不显式指定，`WasmAotEvaluator::new(...)` 走 wasmtime backend
是合理 default。

### 4. 与决策 2（stdlib self-contained）配合

stdlib 全编进 wasm 模块 → wasm 模块自包含 → runtime 选择不影响
stdlib 行为。换 runtime 不会让 stdlib 有差异，只是执行速度 / 启动
延迟不同。

### 5. 与浏览器集成的连续性

当前 `crates/relon-wasm` cdylib 已经在浏览器跑通（playground）。
那条路径**复用同样的 trait abstraction**——浏览器场景就是
`WasmRuntime` trait 的另一个 impl（用 wasm-bindgen 走 JS API）。
这样 playground 后续可以测试新的 wasm AOT codegen 输出，也跑同一份
runtime trait。

## `trait WasmRuntime` 草案

```rust
// relon-codegen-wasm/src/runtime.rs

pub trait WasmRuntime: Send + Sync {
    type Instance: WasmInstance;

    fn instantiate(
        &self,
        module_bytes: &[u8],
        imports: HostFnImports<'_>,
        cap_grants: &Capabilities,
    ) -> Result<Self::Instance, InstantiateError>;
}

pub trait WasmInstance: Send + Sync {
    /// Call exported run_main.
    fn run_main(
        &mut self,
        in_buf: &[u8],
        out_buf: &mut Vec<u8>,
    ) -> Result<usize, WasmTrap>;

    /// Read raw bytes from wasm linear memory at offset.
    fn read_memory(&self, offset: u32, len: u32)
        -> Result<&[u8], OutOfBounds>;

    /// Write raw bytes to wasm linear memory at offset.
    fn write_memory(&mut self, offset: u32, bytes: &[u8])
        -> Result<(), OutOfBounds>;
}

pub struct WasmTrap {
    pub code: TrapCode,
    pub bytecode_offset: u32,
}
```

### Default impl

```rust
// relon-codegen-wasm/src/runtime_wasmtime.rs (feature-gated)

#[cfg(feature = "runtime-wasmtime")]
pub struct WasmtimeRuntime {
    engine: wasmtime::Engine,
}

impl WasmRuntime for WasmtimeRuntime {
    type Instance = WasmtimeInstance;
    // ... wraps wasmtime API ...
}
```

feature gate 让"只在浏览器跑"的 host 可以 disable wasmtime dep；
"只在后台跑"的 host 可以 disable browser-specific stuff。

### Browser impl

```rust
// crates/relon-wasm/src/runtime_browser.rs (现有 crate 扩)

#[cfg(target_arch = "wasm32")]
pub struct BrowserWasmRuntime { ... }

impl WasmRuntime for BrowserWasmRuntime {
    // ... wraps wasm-bindgen-generated JS API ...
}
```

### `WasmAotEvaluator` 是泛型

```rust
pub struct WasmAotEvaluator<R: WasmRuntime> {
    instance: R::Instance,
    srcmap: SrcMap,
    abi: AbiMetadata,
}

impl<R: WasmRuntime> WasmAotEvaluator<R> {
    pub fn compile_and_load(
        ws: &WorkspaceTree,
        runtime: &R,
        ctx: &Context,
    ) -> Result<Self, ...> { ... }
}

impl<R: WasmRuntime> Evaluator for WasmAotEvaluator<R> {
    // implements trait Evaluator over the runtime
}
```

Default type alias 给 host 一行用：

```rust
pub type WasmAotEvaluatorDefault = WasmAotEvaluator<WasmtimeRuntime>;
```

## 跨 runtime 的功能 parity 表

| 功能 | wasmtime | wasmer | wasm3 | browser |
| --- | --- | --- | --- | --- |
| linear memory r/w | ✅ | ✅ | ✅ | ✅ |
| host fn import | ✅ | ✅ | ✅ | ✅ |
| trap with bytecode offset | ✅ | ✅ | ✅ | ⚠️ (via DevTools API) |
| custom section read | ✅ (wasmparser) | ✅ | ✅ | ✅ (WebAssembly.Module.customSections) |
| ahead-of-time compile cache | ✅ | ✅ | ❌ | ❌ |
| 32-bit linear memory | ✅ | ✅ | ✅ | ✅ |
| 64-bit linear memory (`wasm64`) | ✅ | ✅ | ❌ | ⚠️ (experimental) |

v1 走 32-bit memory（决策 1 binary layout 的 offset 都用 u32）。

## Consequences

正面：

- host 自由选 runtime，按场景优化
- 浏览器场景与后台场景共享 codegen，差只在 runtime 层
- 未来加新 runtime（如 wasmedge、wasmer-edge）成本 = 100-200 行 trait impl

负面：

- trait abstraction 加一层间接调用——但 wasm runtime 的 startup
  cost 已经 >> trait dispatch overhead，可忽略
- 多个 runtime impl 需要 feature gate 防止 dep 全堆进每个 host
- 跨 runtime 行为差异（trap message 格式 / 加载错误码）需要 normalize
  到 `WasmTrap` 统一形状

## 测试覆盖

Phase 8 实施时：

- wasmtime backend：所有 fixture 跑通
- browser backend（通过 relon-wasm cdylib + JS test）：所有 fixture 跑通
- runtime mismatch 检测：用 wasmer 加载 wasmtime-compiled module，
  ABI section 兼容 → 成功跑（验证 portability）
- 故意 corrupted module：所有 runtime 都报合理错误（不 crash）

## 暂时不做（v2+）

- wasmtime 的 ahead-of-time compile cache（`Engine::serialize_module` /
  `deserialize_module`）——这是优化 cold-start 的好钩子，但 MVP 不
  需要
- wasm component model 支持（参考 ADR-C，C2 路径推迟）
- wasm64 内存模型（offset 用 u64）—— v2 ABI bump 才可能
- runtime-level resource limits（fuel / epoch interrupt）——可选项，
  跟 `Capabilities::max_steps` 结合时再设计

## 结语 + Phase 0 收尾

本 ADR 是 Phase 0 第 8 份也是最后一份产出。`#main` 入口的 binary
handshake、stdlib bundling、custom section srcmap、static topo eager
四个核心决策 + 五个 ADR（closure boundary / host fn schema / multi-file
import / schema validation site / runtime choice）+ 一份 crate
structure ADR = **9 份文档**完整覆盖 wasm 后端 v1 设计空间。

进入 Phase 1 实施前还需要：

- 把"待定子问题"里的 syntax 选择（ADR-B `#native` 升级 vs
  `#import from "host"`）拍板
- workspace Cargo.toml 加 `relon-ir` / `relon-codegen-wasm` 两个 empty
  crate 骨架
- Phase 1 smoke test 范围确认（极小 `#main(Int) -> Int : x * 2`）

这些是 Phase 0.5 的事，不在本 ADR 范围。
