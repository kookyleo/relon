# 简介：什么是 Relon？

Relon 是一种**可执行的数据格式**：它的载荷就是业务逻辑本身。把校验
规则、计费公式、工作流、风控策略写成 Relon 文档，存进数据库或配置文
件，**任何符合规范的运行时（Rust、Go、TS、Swift、浏览器 WASM…）拿到
同一份源码 + 同一份输入，都会给出字节级一致的结果。**

仓库里的 Rust 实现是参考运行时；语言规范本身和具体 runtime 无关。

> **一句话定位**：Logic as portable data — 把逻辑像 JSON 一样写一次、
> 存一次、传一次，跨端执行结果完全确定。

## 这意味着什么（设计上的硬约束）

* **同源 + 同输入 → 同输出**：Dict 迭代序由 `BTreeMap` 保序、浮点
  走 IEEE-754 `f64`、不读环境变量、不依赖系统时间或 RNG。
* **沙箱默认，无逃生口**：脚本零环境特权；FS、网络、原生函数都需
  要由 `Capabilities` 显式授权——没有「trusted 模式」让脚本绕过宿
  主的允诺。
* **标准库是规范**：`std/list`、`std/string`、`std/math`… 是规范
  的一部分，所有 conformant 运行时必须按相同语义实现。脚本不依赖
  任何 runtime-private 的全局名。

<figure style="margin: 2rem auto; max-width: 720px; text-align: center;">
  <img src="/positioning.svg" alt="Relon 的双层作者模型示意图" style="width: 100%; height: auto;" />
  <figcaption style="margin-top: 0.75rem; font-size: 0.9rem; color: #64748b; font-style: italic;">双层作者模型：平台团队产出词表，业务团队拼装词表。</figcaption>
</figure>

## Relon 是什么

把 Relon 当成「为业务配置量身定做的小型 toolkit」最直观：

- **JSON-like 语法**：写起来像加了表达式、指令、装饰器、引用的 JSON。习惯 JSON 的人 5 分钟上手。
- **类型化 schema**：`#schema` 定义契约，支持 sum-type tagged enum、递归 schema、自定义校验消息、计算默认值。
- **宿主扩展**：Rust 端注册原生函数和装饰器；`.relon` 端写共享 schema/帮助函数；两侧通过 `#import` 拼起来。
- **沙箱默认安全**：`Capabilities` 控制文件读、求值预算、value 大小、原生函数白名单。

## 谁在写什么——双层作者模型

Relon 的设计假设有两类作者：

| 角色 | 工作产出 | 关心的事 |
| --- | --- | --- |
| **平台 / 框架团队** | Rust 扩展（原生函数、装饰器插件） + `.relon` 库（不带 `#main`） | 暴露稳定的「业务词表」，把领域规则编码进 schema 与装饰器 |
| **业务 / 产品团队** | `.relon` 入口文件（带 `#main(...)`） | `#import` 平台库，写贴近 JSON 的配置组合，错误尽早被类型/校验拦截 |

文件是否带 `#main(...)` 决定它的用法：带 `#main` 的是「入口程序」
（必须由宿主推参数才能跑）；不带 `#main` 的可以被宿主直接求值，也
可以被其它文件 `#import`——后者是平台团队库文件最常见的形态。

## 30 行带你走一遍

以下示例覆盖了 `#schema`、sum-type tagged enum、计算默认值、宿主接入。

**`platform/notify.relon`**（平台团队的库，无 `#main`）：

```relon
{
    // 通知通道：sum-type tagged enum
    #schema Channel Enum<
        Email { String address: *, String subject: * },
        SMS   { String phone: * },
        Push
    >,

    // 业务通用的「带主体的通知」契约 + 计算默认值
    #schema Notification {
        Channel via: *,
        String title: *,
        #default (self) => "[" + self.title + "]"
        String summary: *
    }
}
```

**`app/main.relon`**（业务团队的 entry）：

```relon
#import * from "../platform/notify.relon"
{
    Notification welcome: {
        via: Channel.Email { address: "user@x.com", subject: "Hi" },
        title: "Welcome"
    }
}
```

宿主端三行 Rust 拿到 JSON：

```rust
let json = relon::json_from_file("app/main.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

输出（注意 `Email` 这一层是 sum-type 的**外部标签**形式）：

```json
{
  "welcome": {
    "via": { "Email": { "address": "user@x.com", "subject": "Hi" } },
    "title": "Welcome",
    "summary": "[Welcome]"
  }
}
```

## Relon 不是什么

为了避免误解，下面这些**不在路线图上**：

- ❌ **多格式输出**：不会生成 YAML/TOML/XML——[Pkl](https://pkl-lang.org/) 是可以的。
- ❌ **通用脚本语言**：没有 IO、没有循环语句、没有副作用——不要拿来替代 Lua/Starlark。
- ❌ **纯约束验证器**：Relon 既描述也求值；只想做约束，[CUE](https://cuelang.org/) 更合适。
- ❌ **总函数 / 纯函数主义**：求值会失败、closure 不强求 totality——这不是 [Dhall](https://dhall-lang.org/)。
- ❌ **跨语言原生类型/装饰器注册**：v1 路线图里只有 C ABI 的「JSON 进 JSON 出」入口，外加 native-fn 通过 JSON-wire 回调；不会做 Python/Node 端的 schema 注册。
- ❌ **多环境分支原语**：没有 `dev/staging/prod` 切换关键字——用普通的 `match` / `if` 表达式即可。

## 下一步去哪儿

- 语法基础：[基础语法](./syntax.md)
- 写契约：[类型与契约（Schema）](./types.md)
- 模块与入口程序：[模块与作用域](./modules.md)
- 嵌入到 Rust 宿主：[嵌入宿主](./host-integration.md)
- 跑不可信脚本：[沙箱与权限](./sandbox.md)
- 标准库一览：[标准库](./stdlib.md)
