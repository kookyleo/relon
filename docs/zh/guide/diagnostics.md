# 诊断契约

Relon 通过面向人的 CLI 输出报告 parse、analysis、workspace、runtime、
backend、format、compatibility 失败；底层错误带 source span 时使用
`miette` diagnostic。

首个公开版本**不承诺**机器可读 JSON diagnostics。如果以后需要，会作为
单独 CLI 选项设计。

## CLI exit code

| Exit code | 含义 |
| --- | --- |
| `0` | 命令成功。 |
| 非 `0` | parse、analysis、runtime、backend、format、check 或 host-policy generation 失败。 |

首版不承诺为每类错误分配不同 exit code。

## Diagnostic namespace

稳定 namespace 形态如下：

| Namespace | 来源 |
| --- | --- |
| `relon::parse::*` | Parser 失败。Parser 错误目前可能由 CLI 文本包装，而不是 miette code。 |
| `relon::analyze::*` | 单文件语义分析：类型、schema、reference、函数调用、capability 诊断。 |
| `relon::workspace::*` | 多文件加载与 import graph 诊断。 |
| `relon::eval::*` | runtime 求值、资源限制、capability denial、validation、import hash、`#main` 调用错误。 |

随着语言发展可以新增具体 code，但必须留在这些 namespace 内；新增子系统
需要先在本页记录。

## 位置语义

有 source span 时，诊断应指向用户源码。跨模块诊断在入口模块报告 import
位置，并在文本中说明相关 imported path。

Backend compatibility 错误必须说明被选择的后端，以及源码是被拒绝、由
`auto` 路由回 tree-walk，还是被编译后端判为 unsupported。

## 资源错误

执行限制错误在后端知道具体值时应携带结构化信息：

| 错误 | 必要信息 |
| --- | --- |
| `relon::eval::step_limit_exceeded` | 执行路径知道时携带 `limit`。 |
| `relon::eval::value_too_large` | `limit` 和 `actual`。 |
| CLI output-byte rejection | 序列化字节数和配置上限。 |
| Wasmtime fuel / epoch / memory trap | 宿主/runtime 应映射成操作者可读错误。 |

编译后端有时只知道 guard trap 发生了；这种情况下仍应说明 trap 类别，即
便无法给出精确消耗量。

## 常见失败样例

CLI contract 测试会运行这些 fixture 形态，并把规范化后的输出与 golden
对比。下面展示稳定意图；首版仍不承诺 JSON 输出。

### Parse error

```relon
{ a: }
```

```sh
relon check parse.relon
```

期望类别：由 analyzer 包装的 parse 失败，文本中说明 `expected
expression`。

### 静态类型不匹配

```relon
{ Int port: "oops" }
```

```sh
relon check type.relon
```

期望类别：`relon::analyze::*` 或 analyzer 文本；应指向 `port`，并报告
`expected Int, value is String`。

### Schema validation failure

```relon
#schema C { #expect "n positive" Int n: (Int n) -> Bool => n > 0 }
#main(C c) -> C
c
```

```sh
relon run --backend tree-walk schema.relon --args '{"c":{"n":0}}'
```

期望类别：`relon::eval::main_arg_type_mismatch`，包含 `#main` 参数名、
schema constraint、实际值，并把 source span 指向参数声明。

### Capability / import policy denied

```relon
#import x from "https://example.com/a.relon"
{ y: 1 }
```

```sh
relon check remote_import.relon
```

期望类别：analyzer/workspace 文本说明 remote `#import` 需要 `--trust`
或 `Capabilities::network`。这是 capability 姿态失败，不表示 OS sandbox
边界。

### Step limit exceeded

```relon
#relaxed
{ loop(): loop(), x: loop() }
```

```sh
relon run --backend tree-walk --max-steps 10 steps.relon
```

期望类别：`relon::eval::step_limit_exceeded`，source span 靠近耗尽预算的
递归调用，并在 help 文本中说明 `max_steps`。

### Backend unsupported

```relon
{ x: 1 }
```

```sh
relon check --backend cranelift-aot backend_unsupported.relon
```

期望类别：backend compatibility 失败；必须说明 `cranelift-aot` 以及原因：
Cranelift AOT 需要 `#main(...)`。

### 缺少 `#main` 参数

```relon
#main(Int x) -> Int
x
```

```sh
relon run missing_arg.relon
```

期望类别：invocation 失败；说明声明了 `#main(...)` 的文件需要
`--args '<json>'` 或 `--args -`。

## 可恢复性

- parse error 会在 analysis 前终止当前命令。
- analysis error 会阻止 evaluation；普通宿主不应运行已有 diagnostics 的脚本。
- runtime/backend error 终止当前 invocation。
- `relon check` 永不求值；它只 parse、analyze 并报告 backend compatibility。
- formatter check 失败不会改写文件。
