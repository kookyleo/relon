# Playground Design (2026-05-12)

> 设计文档，对应 `docs/zh` + `docs/en` 下计划新增的 in-browser
> playground 特性。
> Wave 1 落地：`relon-wasm` crate（本文 §1）+ 调研结论（§2-§5）。
> Wave 2 / 3 实施前应回到这份文档审一次决策；如果 §2 jsonui 含义
> 未定，**Wave 2 不能开工**。
>
> Snapshot 性质，按 [`README.md` retention policy](./README.md#retention-policy)
> 处置：决策被回流进 roadmap.md 后转为历史快照。

## 1. relon-wasm 现状

- crate path: `crates/relon-wasm/`
- crate types: `["cdylib", "rlib"]`
  - `cdylib` 是 `wasm-pack` / `wasm-bindgen` 工具链消费的产物；
  - `rlib` 让 native 单测能直接 `use relon_wasm::*`，省掉一个
    专门 wasm test runner。
- `publish = false`：API 形状还要等 UI 实施回流（错误协议、entry
  约定）才稳定，先内部用。

### 1.1 暴露的函数

```rust
#[wasm_bindgen]
pub fn evaluate(sources: JsValue, entry: &str) -> Result<JsValue, JsValue>;

#[wasm_bindgen]
pub fn format(content: &str) -> Result<String, JsValue>;

#[wasm_bindgen]
pub fn version() -> String;
```

- `evaluate(sources, entry)`
  - `sources`：JS 端两种合法形态：
    - 对象：`{ "main.relon": "...", "lib.relon": "..." }`
    - 数组：`[{ "path": "main.relon", "content": "..." }, ...]`
      —— 给 Vue `v-for` 一种稳定顺序的形态；
  - `entry`：必须是 `sources` 中某个 path；
  - 成功：返回 projected JSON（plain object / array / scalar）；
  - 失败：抛出 `ErrorReport` JSON（见 §1.4）。
- `format(content)`：透传 `relon_fmt::format_source`，失败返回带
  `ParseError` kind 的 `ErrorReport`。
- `version()`：`env!("CARGO_PKG_VERSION")`，UI 页脚 / cache-buster
  使用。

### 1.2 实测 wasm size

| profile | size (bytes) | size (human) |
| --- | --- | --- |
| `release` | 1 711 305 | 1.63 MiB |
| `release-small` | 1 265 782 | 1.21 MiB |

构建命令：

```bash
cargo build -p relon-wasm --target wasm32-unknown-unknown --release
cargo build -p relon-wasm --target wasm32-unknown-unknown --profile release-small
```

注：这是**链接后**真实的 `.wasm` 体积（cdylib），不是
[`perf-baseline-2026-05-12.md`](./perf-baseline-2026-05-12.md) §
"WASM build sanity" 里那张 rlib 表。`release-small` 比 `release`
省 26%。
后续可走 `wasm-opt -Oz` + `wasm-strip` 再压一遍（Wave 2 引入
`wasm-pack` 流水线时一起做）。

构建中遇到的一个非 crate-内问题：宿主全局 `~/.cargo/config.toml` 里
配了 `[build] rustflags = ["-C", "link-arg=-fuse-ld=mold"]` 来加速
native link；`rust-lld`（wasm32 链接器）不识别 `-fuse-ld=mold`，
cdylib link 阶段直接报错。为了让 `cargo build -p relon-wasm
--target wasm32-unknown-unknown` 在带这种全局配置的开发机上也能开箱
即用，加了项目级 `/.cargo/config.toml`：

```toml
[target.wasm32-unknown-unknown]
rustflags = ["--cap-lints=warn"]
```

`--cap-lints=warn` 是 cargo 默认行为，无副作用；只用来把这一节锚成
"非空 rustflags 覆盖"（cargo 文档：每一层 rustflags 是**互斥**的，
target-section 必须非空才能盖掉 `[build]`）。

### 1.3 模块解析：InMemoryModuleResolver

`crates/relon-wasm/src/lib.rs` 自实现一个 `InMemoryModuleResolver`，
实现 `relon_evaluator::module::ModuleResolver` trait：

```rust
struct InMemoryModuleResolver {
    sources: HashMap<String, String>,
}

impl ModuleResolver for InMemoryModuleResolver {
    fn resolve(&self, path, scope, _) -> Result<Option<ModuleSource>, _> {
        if path.starts_with("std/") { return Ok(None); } // 让 StdResolver 处理
        // 1. 精确 path 匹配；2. 与 scope.current_dir 拼接后再查一次。
        // 没有 std::fs 调用，wasm32 安全。
    }
}
```

Resolver chain（在 evaluate 内组装，**不**用 facade 的
`Sandboxed`/`Trusted` 预设，因为它们都不含 in-memory）：

```
[ InMemoryModuleResolver, StdModuleResolver ]
```

`ResolverChainLoader::from_resolvers(...)` 把同一组喂给 analyzer
的 workspace pass；evaluator 端通过 `Context::prepend_module_resolver`
插在 `Context::sandboxed()` 默认的 `StdModuleResolver` 前面。两边
看到的解析顺序严格一致，避免 analyzer-pass 通过、eval-pass 找不到
模块的撕裂。

Capabilities 走 `Context::sandboxed()` 默认值（reads_fs / writes_fs /
network / clock / env / rng 全 0），**从不**翻成 `all_granted`。
playground 不暴露 `--trust`。任何 capability-gated host fn 调用
会以 `EvalError + CapabilityDenied` 形式失败，这是 demo 正确行为。

### 1.4 错误协议：ErrorReport JSON

```json
{
  "kind": "EvalError",
  "message": "...",
  "spans": [
    { "file": "main.relon", "start": 12, "end": 18, "label": "unresolved" }
  ],
  "help": "...",
  "code": "relon::analyze::unresolved_reference"
}
```

- `kind`: `"InvalidInput" | "ParseError" | "AnalyzeError" | "EvalError" | "ProjectionError"`
- `spans[*].file`: 多文件 playground 场景下用来路由 marker 到对应 tab；
  workspace-level 错误（cycle 等）`file: null`
- `help` / `code`: miette `Diagnostic::help()` / `code()` 的字符串化

JS 端约定：`evaluate(...)` 的 promise 拒绝时，rejection 值就是上面
那个 JSON 对象（而不是 `Error` 实例）。UI 直接 `try / catch (e)` 然后
`e.kind` switch 即可。Wave 2 实施 wasm 包装层时落实这一点。

### 1.5 本地 native 单测

`#[cfg(test)] mod tests` 在 lib.rs 里，7 个测试：

- `evaluates_single_file_arithmetic`：基本算术 → JSON
- `evaluates_two_file_import`：`#import lib from "./lib.relon"`
  跨文件求值
- `parse_error_surfaces_as_parse_kind`：entry-level parse 失败 →
  `kind: ParseError`
- `missing_entry_is_invalid_input`：entry 不在 sources map →
  `kind: InvalidInput`
- `fs_import_denied_in_sandbox`：未注册的相对路径 → `kind:
  AnalyzeError`（沙箱不挂 FilesystemResolver，相对 path 找不到模块）
- `format_passes_through_relon_fmt`：format 烟测
- `version_matches_cargo_pkg_version`：version() ↔ CARGO_PKG_VERSION

`wasm-bindgen-test` 不引入，避免新增 wasm-test-runner toolchain；
当前 7 个测试都是不带 `JsValue` 的内部函数（`evaluate_internal`）
路径，对 wasm 包装的实际验证留给 Wave 2 浏览器联调。

## 2. "jsonui" 调研结论

### 2.1 项目内现状

- `grep -ri "jsonui\|json-ui\|json_ui" /ext/relon/` 命中 **0**
  条（excluding node_modules）。本项目代码、文档里没有 jsonui 这个术语。
- `docs/en/guide/use-cases.md` 列了 8 大场景，**场景 1 "Template
  (amplifier)"** 写："Backend ships minimal data; Relon renders
  complex UI descriptions" —— 是项目自己已经在说的 narrative。
- `docs/en/guide/types.md` 用 sum-type 的 match 演示了"把 variant
  渲染成 UI 字符串"，但仍是 string-level，不是 DOM-level。
- 没有任何现成的"把 Relon eval 出的 JSON 直接渲染成 HTML"的实现/
  组件。

### 2.2 三个可能的解读

| 解读 | 含义 | 落地形态 | 心理模型 |
| --- | --- | --- | --- |
| **A** | 第三方 lib：[JSON Forms](https://jsonforms.io) / [`@jsonforms/vue`](https://github.com/eclipsesource/jsonforms) 等 schema-driven form 渲染器 | Wave 2 引入 `@jsonforms/vue` + `@jsonforms/vue-vanilla`，把 Relon eval 输出当作 `data`、把 `#schema` 元数据投影成 JSON Schema 当 `schema` | "playground 输出可以**填表**" — 弱：需要把 Relon schema 翻成 JSON Schema |
| **B** | 自建：基于 Relon 自己的 `#schema` 元数据写一个最小渲染器（递归遍历 schema-rooted Value，按 field 类型渲染输入框 / 列表 / record）| Wave 2 写 `<RelonRendered :value="..." :schema="..." />` 组件（< 300 行 Vue），零第三方 | "playground 是 Relon schema-rooted 能力的 demo" — 强：直接展示项目核心叙事 |
| **C** | 别的意思（用户的私有缩写、或者特指某个我没识别出来的库） | 等用户澄清 | — |

### 2.3 当前倾向（**未决，需用户拍板**）

倾向 **B**。理由：

1. 项目自我定位 = "Schema-rooted 调用模型"
   （[`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md)），
   schema 是一等公民。一个 demo playground 用自己的 schema 元数据
   驱动 UI 渲染，正好是项目核心 narrative 的"具象证据"；
2. JSON Forms 这类库的 schema 语义（JSON Schema draft 7+）与 Relon
   `#schema` 不一一对应（Sum / Enum / Schema composition 在 JSON
   Schema 里要走 `oneOf` 等绕一圈），翻译层会泄漏到 UI；
3. 自建 ~200 行 Vue + recursive component，可控、可解释、可演化。
   实现成本低于学习并桥接 JSON Forms。

但 **A** 也有合理面：现成、有人维护、表单 UX 已经打磨过。如果
project 长期想做"配置编辑器"而不仅仅是 playground 演示，A 更合理。

> **决策卡点**：等用户在 Wave 2 立项前回答："jsonui 是指
> A / B / C 中的哪个？" 如果选 C，给一句具体含义。

### 2.4 无关 jsonui 解读的 UI 共识

右侧 panel 的 view toggle 至少有：

- `"JSON"` mode：syntax-highlighted JSON 输出（用 Shiki 的 vitepress
  内建主题，没新依赖）；
- `"Rendered"` mode：上面 A / B / C 选定后的 UI 渲染。

`"Rendered"` mode 只在 entry 文件能成功 evaluate 出 JSON 且 schema
信息可获取时启用，否则 disabled。

## 3. Editor 选型

| 选项 | bundle 体积 | Vue / Vite 集成 | 语法高亮 | LSP-类智能 | 适合 |
| --- | --- | --- | --- | --- | --- |
| Monaco | ~2 MB gz | 中（需要 `vite-plugin-monaco`）| 内置 | 完整 LSP | 重 IDE 体验 |
| **CodeMirror 6** | ~200 KB gz | 高（pure ES module） | 自定义 mode 可拼装 | 通过 `@codemirror/lint` 接口 | **推荐** |
| `<textarea>` | 0 | trivial | 无 | 无 | MVP / fallback |

**推荐：CodeMirror 6**。理由：

- 体积友好（playground wasm 1.6 MB 已经偏重，editor 再加 2 MB Monaco
  对首次加载不友好）；
- VitePress 走 Vite，CodeMirror 6 是 pure ESM、零 toolchain 配置；
- 错误 marker 走 `@codemirror/lint` 的 `Diagnostic[]` 模型，正好喂
  §1.4 的 `ErrorReport.spans[]`，零适配；
- Relon 语法高亮可以先用 `@codemirror/legacy-modes/mode/simple-mode`
  写个 ~50 行 token rule（keywords / strings / numbers / comments），
  Wave 2 视用户反馈再考虑写 Lezer grammar。

文件 tab 切换不依赖 editor 内部 model：playground 自管
`PlaygroundFile[]`，editor 在 active file 变化时 `setState` 一份新
`EditorState`（CodeMirror 6 推荐做法，多 buffer 用多 state instance）。

## 4. UI 模型

### 4.1 数据形状

```ts
interface PlaygroundFile {
  path: string;          // "main.relon", "lib.relon"
  content: string;
}

interface PlaygroundState {
  files: PlaygroundFile[];
  activeFile: string;          // path
  entry: string;               // evaluate 入口；可与 activeFile 解耦
  viewMode: "json" | "rendered";
  result: unknown | null;      // 上一次 evaluate 成功的 projected JSON
  errors: ErrorReport[];       // 同 §1.4
}
```

`entry` 与 `activeFile` 解耦：用户可能在编辑 `lib.relon` 但 entry
始终是 `main.relon`。UI 在 tab 栏挂一个"set as entry"按钮（默认
entry 是第一个 file）。

### 4.2 布局示意

```
+--------------------------------+--------------------------------+
| [main.relon*] [lib.relon] [+]  | [JSON] [Rendered]    [Format]  |
+--------------------------------+--------------------------------+
|                                |                                |
|  // CodeMirror 6               |  { "price": 100, ... }         |
|  // active file content        |  // Shiki-highlighted JSON or  |
|                                |  // RelonRendered component    |
|                                |                                |
+--------------------------------+--------------------------------+
| Errors (2)                                                      |
|  main.relon:12  Unresolved reference 'price_2'  (E_UNRESOLVED)  |
|  main.relon:18  Type mismatch: expected Int, got String         |
+-----------------------------------------------------------------+
```

`*` 表示 entry。`[+]` 添加新文件 tab。`Format` 按钮调
`relon_wasm.format(activeFile.content)`。

### 4.3 错误展示策略

- CodeMirror 6 gutter marker：`@codemirror/lint` 的 `Diagnostic`
  数组，把 `ErrorReport.spans[]` 中 `file == activeFile` 的项映射
  过去；
- 底部错误 panel：列全部错误（包括非 activeFile 的），点击跳转到
  对应 file + 位置；
- workspace-level（`file: null`）错误：只显示在底部 panel，不打
  gutter。

### 4.4 wasm 异步加载

VitePress 是 SSG，构建期不能跑 wasm。包装层：

```ts
// playground/wasm.ts
let mod: typeof import("relon-wasm") | null = null;
async function loadWasm() {
  if (!mod) mod = await import("relon-wasm");
  return mod;
}
```

页面 client-side `onMounted` 触发 `loadWasm()`，期间 UI 显示
"Loading runtime (1.6 MB)..."。后续重渲染都命中已加载实例。

## 5. Wave 2 / Wave 3 实施拆分

### Wave 2（VitePress 组件实施，prerequisite: §2 决策已定）

| # | 任务 | 量级 | 依赖 |
| --- | --- | --- | --- |
| 2.1 | `wasm-pack build --target web` 流水线 + `relon-wasm` 包装为 npm-importable ES module | 0.5d | — |
| 2.2 | CodeMirror 6 + Relon simple-mode 高亮 → Vue 组件 `<RelonEditor />` | 1d | — |
| 2.3 | `<Playground />` 组件骨架（左右两栏、文件 tab、view toggle、entry 切换）| 1d | 2.2 |
| 2.4 | wasm bindings 接入：`evaluate` / `format` / `version`，错误 → `Diagnostic[]` | 0.5d | 2.1 |
| 2.5 | 错误显示：CodeMirror gutter + 底部 panel | 0.5d | 2.3, 2.4 |
| 2.6 | JSON 视图：Shiki 高亮的 read-only block | 0.5d | 2.3 |
| 2.7 | Rendered 视图：按 §2.3 决策（A / B / C） | 1d | §2 决策 |

### Wave 3（docs 页 + sidebar + preset）

| # | 任务 | 量级 | 依赖 |
| --- | --- | --- | --- |
| 3.1 | `docs/zh/playground.md` 中文引导页 + 嵌入 `<Playground />` | 0.5d | Wave 2 |
| 3.2 | `docs/en/playground.md` 英文引导页 | 0.5d | Wave 2 |
| 3.3 | VitePress sidebar 加 entry（zh + en 同步） | 0.5d | 3.1, 3.2 |
| 3.4 | 预置示例：`demo` / `pricing` / `feature_flag` / `workflow` 四组源码作为可切换 preset | 1d | Wave 2 |
| 3.5 | 部署：GitHub Pages / Cloudflare Pages 上 `.wasm` 的 `Content-Type` / `Cross-Origin-Opener-Policy` 配置（如果用 Web Worker） | 0.5d | 3.1 |

## 6. 风险 / 开放问题

1. **§2 jsonui 含义未定**。Wave 2 task 2.7 阻塞。
2. **wasm size**：`release` 1.63 MB / `release-small` 1.21 MB，未跑
   `wasm-opt -Oz`。GitHub Pages 默认 gzip，预期上线后 ~500-700 KB
   over the wire。首次加载用户体验可接受（VitePress 本身 ~300 KB），
   但移动网络下提示 "loading runtime..." 不可省。
3. **VitePress 部署平台的 `.wasm` Content-Type**：GitHub Pages 默认
   不发送 `application/wasm` MIME（曾有问题，现已修复，需上线后实测
   验证）。Cloudflare Pages 默认正确。Wave 3 task 3.5 单独验证。
4. **大 source / 长 evaluate 阻塞 UI 主线程**：当前同步 API。多文件
   几百行 Relon evaluate 在桌面端约几 ms（[`perf-baseline-2026-05-12.md`](./perf-baseline-2026-05-12.md)
   `complex.eval` ≈ 7 ms / 1000 elem），wasm 端预计 5-10×，仍在 50
   ms 内，**Web Worker 暂不必要**。如果 Wave 3 预置示例里有更大的
   case 把这个 budget 打爆，再起 Worker 重构 task。
5. **多文件错误路由**：`InMemoryModuleResolver` 用 `entry` 的 parent
   dir 给被 import 模块算 `current_dir`，对 nested `dir/sub.relon`
   导入未做充分测试。Wave 2 task 2.5 联调时重点 cover。
6. **rust-lld 与 mold 冲突**：项目级 `.cargo/config.toml` 已经处理
   （§1.2）；但如果未来引入更多 host-only 的 rustflags（如
   target-cpu=native），需要同步更新这个文件，不能让 wasm32 link
   阶段被波及。
