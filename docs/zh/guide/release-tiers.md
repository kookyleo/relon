# 发布 tier

Relon 仓库里有多条执行相关 crate，但首个公开版本的承诺小于目录结构。
本页是首发支持契约。

## Tier 1：稳定核心

稳定核心是可移植语言 surface：

- parser、analyzer、类型检查、默认严格诊断；
- tree-walk evaluator，作为全 surface 参考实现；
- `relon` facade API，包括默认沙箱入口和显式 trusted 入口；
- CLI `run`、`check`、`fmt`、`host-policy`；
- 文档、formatter、LSP 基础诊断/补全、以及 [标准库](./stdlib.md) 里文档化的
  `std/...` 模块。

用户脚本默认应以这一层为目标。

## Tier 2：默认原生性能路径

Cranelift AOT 是 `Backend::Auto` 和 `relon run --backend auto` 在非平凡
`#main(...)` 程序上的原生性能路径。

承诺：

- 已支持形状必须和 tree-walk oracle 差分一致；
- 不支持形状必须响亮失败，或通过 Auto 的显式 fallback 消息转回
  tree-walk；静默错编是发布阻塞；
- 首个公开版本不支持 `Backend::Auto + TrustLevel::Trusted`。宿主自有
  脚本如果需要 trusted import 或 staged host fn，应使用
  `Backend::TreeWalk`；编译性能路径应显式选择，且不依赖 staged host fn；
- CLI evaluator budget 不会被悄悄忽略：step/value budget 在 `auto`
  下强制 tree-walk，在显式 `cranelift-aot` 下直接拒绝。

CI 如果要确认某文件必须能走编译后端，用：

```bash
relon check --backend cranelift-aot path.relon
```

## Tier 3：高级 / preview

这些 crate 真实存在且有测试，但不是首发默认 surface：

- LLVM AOT：通过 `llvm-aot` cargo feature 和 LLVM 18 工具链 opt-in。
  适合宿主自有的编译部署和 AOT 演进，不是核心路径的通用替代。
- Rust build-time AOT（`relon-rs-*`）：宿主自有、闭世界集成路径。
- object cache / link：原生性能基础设施，不是语言特性。
- 浏览器 wasm bindings / playground：作为文档 playground 产品面支持；
  服务端 untrusted VM 部署应走 Wasmtime 宿主策略。

## 不可信 VM 部署

插件、多租户、外部上传脚本这类场景应使用 VM 或进程边界。Relon 定义
语言侧 capability / budget 模型；真正强制运行时限制的是宿主基础设施。
Wasmtime 可从这里开始：

```bash
relon host-policy --target wasmtime --profile untrusted --format rust
```

边界模型见 [威胁模型](./threat-model.md)；Wasmtime 接线模板见
[Wasmtime 宿主策略](./wasmtime-host-policy.md)。
