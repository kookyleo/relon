# 威胁模型

Relon 首发版本的安全规则很简单：**没有隐式 trust**。所有 trust 姿态
都必须在宿主边界显式出现，并且能在 review 中被审计。

本页是规范性的安全边界说明。其它页面展示 API 旋钮；本页说明它们保证
什么、不保证什么。

## Relon 自己负责什么

Relon 自己承担这些语言层保证：

| 区域 | 保证 |
| --- | --- |
| 确定性 | 语言 builtin 不读取时间、随机数、环境变量、文件、网络或进程状态。 |
| 显式 trust | `--trust`、`*_trusted`、`TrustLevel::Trusted` 都是宿主 opt-in；脚本不能给自己授权。 |
| 能力词汇 | `Capabilities` 命名语言/宿主权限，如文件 import、network、clock、subprocess、受闸门控制的 native fn。 |
| 静态能力诊断 | analyzer 会对静态可见的 gated call 报告缺失授权。 |
| 运行时拒绝 | evaluator/backend 会拒绝未授权 capability bit，而不是静默调用宿主代码。 |
| 正确性 trap | 除零、数值溢出、缺失 `#main` 参数、不支持的后端形状、越界、校验失败都会作为错误浮现。 |

## Relon 单独不负责什么

Relon 不是操作系统沙箱。

| 区域 | 需要的边界 |
| --- | --- |
| 多租户隔离 | 使用 Wasmtime、其它 VM、子进程、容器或进程边界。 |
| 挂钟截止时间 | 使用 Wasmtime epoch interruption，或宿主/进程 timeout。 |
| 硬内存上限 | 使用 Wasmtime `StoreLimits`、OS limit、cgroup 或容器。 |
| 宿主 import 行为 | 逐个审计和包装 import；Relon 只按 capability gate 放行调用。 |
| WASI / 文件系统 / 网络环境权限 | 默认拒绝，只通过宿主 runtime policy 显式授予。 |

## 后端边界

| 后端 | 用途 | 安全边界 |
| --- | --- | --- |
| `tree-walk` | 参考/调试/开发执行；覆盖全语言 surface。 | 进程内 guardrail，不是租户边界。 |
| `cranelift-aot` | 支持形状上的默认原生性能路径。 | 进程内 native code，带 trap 和 capability gate；不是租户边界。 |
| `llvm-aot` | advanced/preview 的宿主自有 AOT 路径。 | 视为链接进宿主进程的 Rust 代码；资源控制属于宿主部署层。 |
| wasm / Wasmtime | 不可信插件、租户、上传脚本的推荐 VM 路径。 | Wasmtime fuel、epoch、`StoreLimits`、import/WASI policy、宿主/进程控制。 |

## Capabilities

`Capabilities` 是 Relon 的权限词汇。它说明从语言/runtime 角度哪些操作
被允许；它不会改变操作系统权限。

例子：

- `reads_fs` 只有在宿主也安装了有意 root 的 filesystem resolver 时，
  才能让 resolver 读文件。
- 带 `NativeFnGate` 的 native fn 只有在对应 capability bit 被授予时
  才可调用。
- `Capabilities::all_granted()` 可以用于宿主自有脚本，但必须在调用点
  显式可见。

## ResourceBudget

`ResourceBudget` 是 Relon 的标准预算模型。这不表示每个后端都能自动执行
同样的硬限制。

| 预算 | 由谁执行 |
| --- | --- |
| 源码字节数 | CLI/SDK 在可取得 metadata 时，于读入/解析前预检。 |
| Tree-walk steps | tree-walk evaluator 计数器。 |
| Value 元素数 | tree-walk value 构造检查（已接入处）。 |
| 输出字节数 | CLI/宿主在序列化边界检查。 |
| Wasm fuel | Wasmtime `Config::consume_fuel` + `Store::set_fuel`。 |
| 挂钟 timeout | 宿主 timer + Wasmtime epoch interruption，或进程 timeout。 |
| 内存/table 限制 | Wasmtime `StoreLimits` 或 OS/container 控制。 |

不可信 VM 接线见 [Wasmtime 宿主策略](./wasmtime-host-policy)。

## 运维规则

宿主自有配置可以使用稳定核心，并在必要时显式 trust。外部上传或多租户
代码必须放在 VM/进程边界后执行，并保持 WASI/import 默认拒绝。
