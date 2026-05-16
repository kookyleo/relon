# ADR-B：用户 host fn schema 怎么编进 wasm（2026-05-16）

> Phase 0 子项 5/8。
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> §二 待定子问题 B "用户 host fn 的 schema 怎么编进 wasm"。
> 依赖：[`wasm-binary-layout-v1`](./wasm-binary-layout-v1-2026-05-16.md)。

## Context

tree-walker 里 host 通过 `Context::register_fn(name, gate, func)` 注册
native fn。函数的 signature（参数类型 / 返回类型 / capability gate）在
**运行时动态注册**——wasm codegen 编译时**未知**具体哪些 host fn 会
被 wire 进来。

但 wasm codegen 又必须：

- 知道该 fn 的**参数 / 返回 layout** 才能生成正确的 binary handshake
- 知道该 fn 的 **capability gate** 才能正确插入 check_cap opcode
- 知道该 fn 是不是"unresolved at compile time"（host-provided）vs
  stdlib bundled

## Decision

**走 import 占位 + analyzer-side schema declaration**：

1. analyzer 阶段已经知道哪些 fn 是 unresolved（既不在 stdlib，也不是
   用户定义）。把这些 fn **必须有显式 schema 声明** 才能通过 strict
   mode（已有约束的强化）
2. wasm codegen 为每个 unresolved fn emit 一个 `import "host" "<name>"`
   占位 + 该 fn 的 layout/cap 元数据写入 `"relon.host_fns"` custom section
3. host SDK instantiate 时读 `"relon.host_fns"`，对每个声明的 import
   名查 `Context.functions`/`native_methods`，验证 host 端注册的 gate
   ⊆ wasm 模块要求的 gate，schema 一致则 link
4. host 没注册 → instantiate 失败，错误信息 `HostFnMissing { name, expected_signature }`

## Rationale

### 1. wasm 模块只能依靠 declared schema

wasm 是静态格式，wasm-codegen 必须**在编译期**生成 import 声明。
import name 必须在 source 里有声明——所以 host fn 在 Relon source
里必须有 forward-declaration 形式。这正是 analyzer "strict mode 拒绝
unresolved fn" 路径已经做的事，只是 wasm codegen 把它显式化。

举例 Relon source：

```relon
#native upper_external(s: String) -> String  ;; 声明 host fn

#main(String s) -> String {
    upper_external(s)
}
```

`#native` 关键字（已存在于 schema method 路径）扩展为顶层
free-fn 声明（v2 新语法，可在 ADR-B Phase 0 输出确认）。声明里写
schema 信息 = analyzer 在 strict 模式下能算 layout。

### 2. host_fns custom section 的内容

```
custom "relon.host_fns":
  magic: [u8; 4] = b"RLNH"
  format_version: u8 = 1
  fn_count: varuint32
  fns: [HostFnDecl; fn_count]

HostFnDecl:
  name: String                            ;; canonical fn name
  name_hash: [u8; 32]                     ;; sha256(canonical_signature)
  required_caps: u8                       ;; capability bit set
  param_count: u8
  params: [(ParamLayout); param_count]    ;; SAME layout encoding as binary handshake
  return_layout: ParamLayout
```

ParamLayout 表达 `(size, align, kind)` 三元组，host SDK 用来比对自己
注册的 `RelonFunction` 的参数类型。

### 3. capability gate 双层防护

- **wasm 内**：在 host-fn import call site 之前 codegen emit `check_cap`
  指令；如果 wasm 模块 instantiate 时传入的 `cap_grants` bitmap 缺位
  就 trap（runtime 拒绝）
- **host 内**：host SDK link 该 import 时强制 `NativeFnGate ⊆ cap_grants`
  ——host 注册的 fn 不允许在没被 grant 的 cap 上跑。这是 instantiate
  期检查，比 wasm trap 更早

两层防护语义等价；instantiate-time check 在加载期就 reject，运行期
trap 是 fallback。

### 4. 与决策 2（stdlib self-contained）不冲突

stdlib `upper` / `range` 等**不走 import**——bundled into wasm
bytecode 直接 call。只有真正 host-provided fn 才在 `host_fns` 表里
出现。所以 host_fns 表通常**很短**（5-20 entries 典型），section
开销可忽略。

### 5. Schema hash 防止 host/wasm mismatch

`name_hash = sha256(canonical_signature)`：

```
canonical_signature(fn) = {
    "name": <fn_name>,
    "params": [
        { "name": <pname>, "type": canonical_type(ptype) }, ...
    ],
    "return_type": canonical_type(ret_type),
    "required_caps": [...sorted cap names...],
}
```

host SDK 在 link import 时算同样的 hash 比对——hash mismatch 直接
报 `HostFnSignatureDrift { name, wasm_hash, host_hash }`。Schema 演化
（host 端改了 fn 签名但 wasm 还按旧签名编）就在 instantiate 期暴露。

## Implementation hints

### `#native` 关键字扩展（语言级改动）

当前 `#native` 只用在 schema method context：

```relon
#schema String with {
    #native upper() -> String
}
```

ADR-B 提议把 `#native` 升级为顶层 free-fn forward-declaration：

```relon
#native upper_external(s: String) -> String

#native_with_caps[reads_fs] read_config(path: String) -> String

#main(...) -> ... {
    upper_external(s)
    read_config("/etc/foo")
}
```

`#native_with_caps[...]` 是新 syntax sugar；analyzer 把 `[...]` 解析
为 `NativeFnGate`。

**回退方案**：如果 syntax 改动阻力大，可以走**declaration-by-comment**
风格（类似 TypeScript ambient types）：

```relon
#import { upper_external: (String) -> String } from "host"
#import { read_config: (String) -> String, caps=[reads_fs] } from "host"
```

`from "host"` 是保留 module 名，analyzer 识别后写入 host_fns 表。
两种方案二选一，Phase 0 后续 ADR 拍板。

### analyzer 侧改动

`relon-analyzer/src/main_signature.rs`：strict mode 检查所有 fn call
site，未在 stdlib + 未在用户定义 + 未在 host_decls 表中 → 报
`UnresolvedHostFn { name, suggest_add_native_decl }`。

### codegen 侧改动

`relon-codegen-wasm/src/host_fn_table.rs`：

```rust
fn emit_host_fn_imports(
    module: &mut wasm_encoder::Module,
    host_decls: &[HostFnDecl],
) -> Vec<u32> {  // returns wasm import indices for codegen to call
    for decl in host_decls {
        module.import(
            "host",
            &decl.name,
            wasm_encoder::EntityType::Function(
                build_wasm_signature(&decl.params, &decl.return_layout)
            ),
        );
    }
    // build the host_fns custom section
    emit_host_fns_custom_section(module, host_decls);
}
```

### host SDK 侧改动

```rust
// relon-codegen-wasm/src/instantiate.rs (草案)

pub fn instantiate(
    module: &WasmModule,
    ctx: &Context,
    cap_grants: &Capabilities,
) -> Result<Instance, InstantiateError> {
    // 1. read host_fns from module
    let declared = module.host_fns()?;

    // 2. validate each declared fn against ctx.functions
    for decl in &declared {
        let host_fn = ctx.functions.get(&decl.name)
            .ok_or_else(|| InstantiateError::HostFnMissing {
                name: decl.name.clone(),
                expected: decl.signature_for_error(),
            })?;

        // 2a. hash check
        let host_hash = canonical_signature_hash(host_fn);
        if host_hash != decl.name_hash {
            return Err(InstantiateError::HostFnSignatureDrift {
                name: decl.name.clone(),
                wasm_hash: decl.name_hash,
                host_hash,
            });
        }

        // 2b. capability subset check
        let missing = decl.required_caps.missing_in(cap_grants);
        if !missing.is_empty() {
            return Err(InstantiateError::HostFnCapNotGranted {
                name: decl.name.clone(),
                missing,
            });
        }
    }

    // 3. link the imports
    let mut linker = wasmtime::Linker::new(&engine);
    for decl in &declared {
        let host_fn = ctx.functions.get(&decl.name).unwrap();
        linker.func_wrap("host", &decl.name, wasm_wrap(host_fn))?;
    }

    // 4. instantiate
    linker.instantiate(&mut store, &module.wasm())
}
```

## Consequences

正面：

- wasm 模块 self-describes 它的 host fn 需求；host SDK 不需要 magic
  knowledge
- schema drift 在加载期就暴露，不是运行时
- capability 双层防护（host instantiate-time + wasm runtime check_cap）

负面：

- 需要语言级新语法 / 约定（`#native` 升级或 `#import from "host"`）
- analyzer 改动量：增加 strict-mode host fn 解析
- canonical signature hash 计算要在 host 和 wasm 两侧实现，并保持
  完全一致（hash mismatch debug 不容易）

## 测试覆盖

Phase 1+ 实施时：

- host 注册 fn 后 wasm `instantiate` 成功 → call host fn → 返回值正确
- host 没注册声明的 fn → `HostFnMissing` with hint
- host 注册的 fn 签名与 wasm 期望不符 → `HostFnSignatureDrift`
- host 注册的 fn 要 `reads_fs` 但 cap_grants 不含 → `HostFnCapNotGranted`
- wasm runtime 内 capability bitmap 不含必要 bit → wasm trap +
  translate 为 `CapabilityDenied`

## 暂未决（留 Phase 0 后续 ADR）

- `#native` syntax 升级 vs `#import from "host"` syntax 选哪个
- host fn 是否支持 generic 签名（`#native foo<T>(x: T) -> T`）——
  v1 倾向**不支持**（要求 host fn 签名 mono；用户层走 dispatch）
