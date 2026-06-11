# Playground 与 wasm 绑定

导航栏的 [Playground](/zh/playground) 是一个完全在浏览器内运行的
Relon 环境：解析、分析、求值、格式化全部由 WebAssembly（一种让
Rust 代码在浏览器里以接近原生速度运行的字节码格式，下称 wasm）模块
完成，**没有任何服务器参与**。

它由两部分组成：

- **绑定层** `crates/relon-wasm-bindings`：用 wasm-bindgen（Rust 与
  JavaScript 之间的绑定生成工具）把 Relon 引擎编译为 wasm，向 JS
  暴露求值、格式化与一组编辑器智能接口。任何想在浏览器里嵌入 Relon
  运行时的页面都可以直接复用这一层。
- **前端** `docs/.vitepress/theme/components/`：基于 CodeMirror
  （浏览器端代码编辑器组件）的多文件编辑器页面，消费上述接口实现
  即时求值、诊断标记、补全、悬停提示等。

本页以代码为准记录绑定层的全部 JS 出口与本地构建方式。

## 通用调用约定

所有接口共享以下约定：

- **`sources`**：内存中的模块表，两种形状都接受——对象
  `{ "main.relon": "…", "lib.relon": "…" }`，或数组
  `[{ path: "main.relon", content: "…" }, …]`（数组形状保留顺序，
  适合按 tab 渲染的前端）。
- **`entry`**：入口文件名，必须是 `sources` 中的一个 key。
- **`line` / `character`**：光标位置，0 起始；`character` 按 UTF-16
  码元计数（与 CodeMirror、LSP 的位置约定一致）。
- **返回值**：Rust 结构一律序列化为普通 JS 对象 / 数组（不是
  `Map`），可直接属性访问与 `JSON.stringify`。
- **错误面**：失败时抛出的不是裸字符串，而是结构化的
  `ErrorReport` 对象，前端可以据此渲染行内标记：

```ts
interface ErrorReport {
  kind: 'InvalidInput'    // 入参结构错误（缺 entry、sources 形状不对等）
      | 'ParseError'      // 入口文件解析失败
      | 'AnalyzeError'    // 分析器在工作区（入口或任一导入模块）报错
      | 'EvalError'       // 求值期运行时错误
      | 'ProjectionError';// 求值成功但结果无法投影为 JSON（闭包、非有限浮点等）
  message: string;        // 人类可读摘要
  spans: {                // 错误锚定的源码区间（字节偏移），可能为空
    file: string | null;  // 所属模块，null 表示工作区级报告
    start: number;
    end: number;
    label: string | null;
  }[];
  help: string | null;    // miette 风格的帮助文本
  code: string | null;    // 诊断码，如 relon::analyze::unresolved_reference
}
```

## 求值与格式化

### `evaluate(sources, entry, args)`

对入口文件求值，返回投影后的 JSON 结果（普通 JS 对象 / 数组 / 标
量）。一个入口同时覆盖两类脚本——声明了 `#main(...)` 的入口程序按
签名解码 `args` 后运行；没有声明的脚本直接对根表达式求值。

`args` 必须是 **JSON 字符串**（如 `JSON.stringify({...})`）、
`null`、`undefined` 或省略。之所以不收 JS 对象：JS 只有一种
Number，`100.0` 过一遍 JS 对象就和 `100` 无法区分，而 `#main(...)`
签名依赖 Int 与 Float 的区分；字符串在 Rust 侧用 `serde_json` 解析
可以无损保留这一信息（`100` → Int，`100.0` → Float）。

`args` 的解码以 `#main(...)` 的参数类型为目标：支持
`Option<T>`（`null`、`"None"`、`{"Some": …}` 等形状）、
`Result<T, E>`（`{"Ok": …}` / `{"Err": …}` 外标签形状）、
`#enum`（单元变体收字符串名，带载荷变体收
`{"变体名": 载荷}`）、`#schema`、`Tuple` / `List` / `Dict` 及其任意
嵌套。

```js
import init, { evaluate } from './wasm/relon/relon_wasm.js';
await init({ module_or_path: './wasm/relon/relon_wasm_bg.wasm' });

// 无参脚本：根表达式求值
evaluate({ 'main.relon': '{ price: 100 + 23 }' }, 'main.relon');
// → { price: 123 }

// #main 入口 + JSON 字符串参数
evaluate(
  { 'main.relon': '#main(Int n) -> Int\nn * 2' },
  'main.relon',
  JSON.stringify({ n: 21 })
);
// → 42

// 多文件 #import（跨模块成员的静态类型不可推导，需 #relaxed）
evaluate(
  [
    { path: 'main.relon', content: '#relaxed\n#import lib from "./lib.relon"\n{ g: lib.hello }' },
    { path: 'lib.relon', content: '{ hello: "hi" }' },
  ],
  'main.relon'
);
// → { g: "hi" }
```

失败时抛出 `ErrorReport`：

```js
try {
  evaluate({ 'main.relon': '{ not closed' }, 'main.relon');
} catch (err) {
  err.kind;    // "ParseError"
  err.message; // 人类可读的错误摘要
  err.spans;   // [{ file, start, end, label }]
}
```

### `format(content)`

把一段 Relon 源码交给 `relon-fmt` 美化，成功返回格式化后的字符串，
解析失败抛出 `ErrorReport`（`kind: "ParseError"`）。

### `version()`

返回绑定 crate 的版本字符串（取自 `CARGO_PKG_VERSION`，跟随
workspace 版本），可用于 UI 页脚或缓存失效。

## 编辑器智能接口

以下接口与 `relon-lsp` 共用同一套分析器实现，但全部由内存中的
`sources` 表驱动，浏览器端无需文件系统即可获得相同语义。

| 接口 | 返回 | 说明 |
| --- | --- | --- |
| `complete(sources, entry, line, character)` | `CompletionResult[]` | 补全候选。入口解析失败时退回容错解析路径，编辑中途也有补全；永不返回 `null` |
| `hover(sources, entry, line, character)` | `HoverResult \| null` | 悬停提示，`markdown` 为提示正文，附带其描述的源码字节区间 |
| `goto_definition(sources, entry, line, character)` | `GotoDefinitionResult \| null` | 跳转到定义，可跨 `sources` 中的模块；落在 `#import` 路径上时跳到目标文件开头 |
| `find_references(sources, entry, line, character, include_declaration)` | `ReferenceLocation[] \| null` | 查找当前文件内的全部引用；`include_declaration` 控制是否包含声明处 |
| `signature_help(sources, entry, line, character)` | `SignatureHelpResult \| null` | 调用参数提示：渲染后的被调签名 + 光标所在参数序号 |
| `document_symbols(sources, entry)` | `DocumentSymbolWire[]` | 文件大纲。每项带 `parent` 索引（指向同一数组），可直接重建符号树 |
| `inlay_hints(sources, entry)` | `InlayHintWire[]` | 行内嵌注（目前为参数名 ghost text），解析失败时返回空数组 |
| `code_actions(sources, entry, line, character)` | `CodeActionWire[]` | 光标处诊断的快速修复列表，无可用修复时为空数组 |
| `prepare_rename(sources, entry, line, character)` | `PrepareRenameResult` | 探测光标处符号是否可重命名；`valid: false` 时 `error` 携带原因 |
| `rename_symbol(sources, entry, line, character, new_name)` | `TextEditWire[]` | 计算重命名的全部文本替换，失败抛 `ErrorReport` |

几个返回形状的字段约定：

- `CompletionResult`：`{ label, kind, detail, apply_snippet }`。
  `kind` 取值 `method` / `field` / `param` / `schema` / `stdlib` /
  `module` / `import` / `reference` / `directive` / `pragma` /
  `decorator` / `keyword`；`apply_snippet` 是 LSP 风格
  `${N:placeholder}` 模板（可调用项展开为 `@currency(${1:symbol})`
  之类），为 `null` 时直接插入 `label`。
- `TextEditWire`（`rename_symbol`、`code_actions.edits` 共用）：
  `{ start_line, start_character, end_line, end_character,
  start_offset, end_offset, new_text }`——同时给出 LSP 式行列与字节
  偏移，调用方择一使用。
- `GotoDefinitionResult`：`{ path, start: { line, character },
  end: { line, character } }`，`path` 是 `sources` 中的目标文件。
- `HoverResult` / `SignatureHelpResult`：位置用字节偏移
  （`range_start_offset` / `range_end_offset`）表达。

## 本地构建

前置条件：Rust 工具链 + `wasm32-unknown-unknown` 目标、
[wasm-pack](https://rustwasm.github.io/wasm-pack/)、Node.js。

```bash
rustup target add wasm32-unknown-unknown

cd docs
npm install
npm run build:wasm   # wasm-pack 构建 crates/relon-wasm-bindings
                     # 产物落在 docs/public/wasm/relon/（已 gitignore）
npm run test:wasm    # Node 冒烟测试（test-node.mjs）
npm run docs:dev     # 本地起 VitePress，导航栏 Playground 即可用
```

两点注意：

- wasm32 目标在 `.cargo/config.toml` 里开了 `+simd128`，与 crate
  `Cargo.toml` 中 wasm-opt 的 `--enable-simd` 配套，**两处必须同步
  增删**——只动一处要么得到无 SIMD 的慢产物，要么被 wasm-opt 校验
  拒绝。
- 该 crate `publish = false`，目前只作为站点资产构建，不发布到
  crates.io / npm。

线上站点由 `.github/workflows/deploy-docs.yml` 在每次推送 main 时
重新构建 wasm 并随 VitePress 一起部署到 GitHub Pages。

## 沙箱边界

浏览器内与宿主嵌入是同一姿势：绑定层用 `Context::sandboxed()` 构
造求值上下文，能力**默认全拒**，并且不向不可信的浏览器用户暴露任
何 `--trust` 式开关。这是 capability 姿态；通用边界模型见
[威胁模型](./threat-model.md)。

- 没有文件系统模块解析器：`#import` 只能命中 `sources` 表中的文件
  或 `std/*` 虚拟模块；引用表里不存在的路径在分析期即报
  `AnalyzeError`，不会落到磁盘。
- 任何触碰真实能力（文件、宿主原生函数等）的调用都会干净地以
  `CapabilityDenied` 失败，呈现为 `EvalError`。

边界声明见 [威胁模型](./threat-model.md)，能力模型的完整说明见
[沙箱与权限](./sandbox.md)。
