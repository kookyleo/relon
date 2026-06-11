# CI 集成

Relon 进入 CI 时应是一组明确的命令行 gate：格式检查、静态/后端兼容性检
查、golden output 检查，以及不可信 VM 部署用的宿主 runtime policy 生成。

## 最小流水线

```bash
relon fmt --check examples/*.relon
relon check --backend auto examples/validation.relon
relon run --backend tree-walk examples/validation.relon > actual.json
diff -u fixtures/golden/success/examples/validation.json actual.json
relon host-policy --target wasmtime --profile untrusted --format rust > relon_wasmtime_policy.rs
```

如果 CI 在本仓库内运行、还没有安装 `relon` binary，用 cargo 形式：

```bash
cargo run -q -p relon-cli -- fmt --check examples/*.relon
cargo run -q -p relon-cli -- check --backend auto examples/validation.relon
cargo run -q -p relon-cli -- run --backend tree-walk examples/validation.relon > actual.json
diff -u fixtures/golden/success/examples/validation.json actual.json
cargo run -q -p relon-cli -- host-policy --target wasmtime --profile untrusted --format rust > relon_wasmtime_policy.rs
```

## 后端 pinning

普通源码兼容性检查使用 `auto`。只有当某文件必须保持在 native 性能路径
时，才显式使用 `cranelift-aot`：

```bash
relon check --backend cranelift-aot path/to/program.relon
```

`relon check` 不运行程序。它只 parse、analyze，并报告所选后端是否接受
该源码。不支持的编译形状必须响亮失败，或通过 `auto` 明确 fallback。

## Golden 输出

入口程序应把宿主输入和期望 JSON 一起钉住：

```bash
relon run --backend tree-walk examples/feature_flag.relon \
  --args '{"user":{"id":"alice-42","region":"eu","plan":"pro","rollout_bucket":17}}' \
  > actual.json
diff -u fixtures/golden/examples_main/feature_flag.json actual.json
```

Tree-walk 是全 surface oracle。Native backend parity 属于后端/fixture
测试；用户 CI 只有在 native execution 是产品契约时才 pin `cranelift-aot`。

## 不可信 VM 部署

外部输入源码还应在 CI 中钉住宿主 runtime policy 模板：

```bash
relon host-policy --target wasmtime --profile untrusted --format rust
```

这些 limits 应随部署代码一起 review。Relon 不会从源码文件推断
container/process/Wasmtime 限制。

## 仓库内 gate

本仓库内的 release gate 是：

```bash
./scripts/verify.sh
npm run docs:build
```

`verify.sh` 会运行格式检查、workspace build、clippy、workspace tests，以
及 fixture/example 格式检查。
