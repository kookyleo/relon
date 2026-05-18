# wasm-AOT v3++ b-2: `#import` 哈希钉绑设计笔记

## 背景

v3+ a-3 落地了远程 `#import "https://..."` 的 ureq 拉取 + 24h 磁盘缓
存 + `--trust` 阀门，但 fetched body 直接被接受。任何 MITM 或后端污染
都没有本地的检测路径。a-3 同时预留了 `RuntimeError::RemoteImportHash-
Mismatch` variant（payload boxed），等本 phase 接入。

本 phase 在不动远程拉取链路前提下，给 `#import` 加上内联完整性钉绑
syntax 与 analyzer 侧的强制校验。

## 语法

```relon
#import lib from "https://example.com/util.relon" sha256:"<64-char-hex>"
#import { add } from "./local.relon" sha256:"<hex>"
```

- 路径字符串后可选 `<algorithm>:"<hex>"` 后缀。空白可有可无。
- 算法标识符目前只接受 `sha256`；其它名字 (`sha512` 等) parser 接收，
  analyzer 走 `ImportHashUnknownAlgorithm` 报错。
- 本地路径同样支持钉绑——文件被改也会哈希不匹配。
- 不写钉绑等价于 v3+ a-3 旧行为，除非 `--require-hash` 启用。

## Parser 改动

- `crates/relon-parser/src/token.rs`
  - 新枚举 `HashAlgorithm`（目前 `Sha256` 一个 variant，预留 `Sha512`
    / `Blake3`）。
  - 新结构 `IntegrityHash { algorithm: Option<HashAlgorithm>,
    algorithm_text: String, hex: String, range: TokenRange }`。
    - `algorithm = None` 表示算法名未知，但 `algorithm_text` 保留原
      identifier 让 analyzer 报错时能 echo 出来。
  - `DirectiveBody::Import` 新增 `integrity: Option<IntegrityHash>`。
- `crates/relon-parser/src/cst.rs`
  - `parse_directive_import` 在 path STRING 后接受可选 `IDENT COLON
    STRING`，缺少 `:` 或 STRING 时 emit 解析错误。
- `crates/relon-parser/src/lower.rs`
  - 在原本的 import-body lowering pass 里加状态机：path STRING 之后
    若看到 IDENT 则记为 algorithm 候选；接着的 COLON / STRING 完成
    一次 `IntegrityHash` 注入。算法名通过 `from_ident` 翻成 Option，
    未知算法保留 verbatim 文本。

## Analyzer 改动

- `crates/relon-analyzer/src/modules.rs`
  - `ModuleImport` 新增 `integrity: Option<IntegrityHash>` 字段，沿
    AST 向下沉到 workspace 的 BFS 队列。
- `crates/relon-analyzer/src/workspace.rs`
  - 新增四种 `WorkspaceDiagnostic`：
    - `ImportHashMismatch { path, algorithm, expected, got, range }`
    - `ImportHashRequired { path, range }`
    - `ImportHashUnknownAlgorithm { path, algorithm, range }`
    - `ImportHashInvalidHex { path, algorithm, expected, got, range }`
- `crates/relon-analyzer/src/workspace_build.rs`
  - `PendingImport` 携带 integrity；`process_import` 在 `loader.load`
    之前做钉绑健康检查（算法已知、hex 长度匹配、`require_hash` 必要
    时缺钉绑直接拒绝），fetch / load 之后对 `loaded.source` 计算实际
    digest 与钉绑对比。
  - 任何不匹配都让 module 不进入 `ws.modules`——下游 analyzer pass
    / evaluator 永远碰不到被污染的 source。
  - sha256 / hex 编码集中在两个本地 helper `compute_digest` /
    `digest_matches` 里，方便未来扩展或在 runtime 再用同一份语义重
    新验证。
- `crates/relon-analyzer/src/lib.rs`
  - `AnalyzeOptions::require_hash: bool` 新字段，默认 `false`。

## CLI 改动

- `relon run --require-hash`：把 `AnalyzeOptions::require_hash`
  打开。CLI 主入口改走 `analyze_entry_with_options`。
- 默认 off，行为与 a-3 一致；推荐生产环境 / CI 显式打开。

## Cache 关系

a-3 把 fetched body 写入 `<cache_dir>/<sha256(url)>.relon`。本 phase
不改 cache 文件格式。因为：

1. 哈希计算的对象是 `.relon` body 本身。
2. 缓存命中后 analyzer 仍会对 body 算一次 sha256 和钉绑对比；本地
   缓存文件被外部篡改也会被钉绑校验抓住——`cache_hit_still_runs_hash_check`
   测试覆盖了这条路径。

## 不做的事（推下一 phase）

- conditional GET（ETag / Last-Modified）—— v3++ b-3。
- multi-algorithm（sha512 / blake3 多 digest 并存）—— v3+++。
- SRI（subresource integrity）拼写兼容 —— v3+++。
- runtime（evaluator）侧的二次哈希校验：当前 workspace pass 已经
  把污染挡在外面，evaluator 永远走的是 analyzed 过的 module 表，无
  需重复算 sha256。除非未来 evaluator 引入跳过 analyzer 的快速路径
  才需要补回这一层。

## 关键决策

- **algorithm 扩展位**：用 `HashAlgorithm` enum + `Option<>` 包裹，
  未知算法不在 parser 层报错，让 analyzer 拿到原始 identifier 给出
  span-aware diagnostic。
- **`require-hash` 放 CLI 不放 facade**：facade（`value_from_str` /
  `value_from_str_trusted`）的存在意义就是 ergonomic default，不应
  该强加策略；想强制的 host 用 `analyze_entry_with_options` 自带
  `AnalyzeOptions::require_hash = true`。CLI 是面向 operator 的，
  那里有显式 flag 才合理。
- **本地路径也接受钉绑**：钉绑对本地 import 同样有效。理由：本地
  文件改动同样会被钉绑校验抓住（refactor 出错时一个有用的安全
  网）。但 `--require-hash` 只针对远程，因为强制每个 `./util.relon`
  写钉绑会破坏日常迭代体验。

## Gate

```
cargo build --workspace
cargo test --workspace --features 'relon/wasm-aot'
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --target wasm32-unknown-unknown -p relon-wasm
```
