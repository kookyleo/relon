# 库与入口（@library）

Relon 假定有两类作者：写「词表」的平台团队，和写「配置」的业务团队。语言层面用 `@library` 标记把这两类文件隔开——这页讲清楚区别、规则和典型用法。

> 如果你只想看怎么写、何时写，跳到[文末的 cheat-sheet](#何时该标-library-何时不标)。

## 双层作者模型

Relon 不假装大家都是同一种作者：

| 角色 | 工作产出 | 在意的事 |
| --- | --- | --- |
| **平台 / 框架团队** | <ul><li>Rust 扩展（原生函数、装饰器插件）</li><li>`.relon` 库（标 `@library`）</li></ul> | 提供稳定的「业务词表」，把领域规则编进 schema |
| **业务 / 产品团队** | <ul><li>`.relon` 入口文件（**不**标 `@library`）</li></ul> | `@import` 平台库；写贴近 JSON 的配置；让类型/校验在边界拦错 |

两类文件的语法没有任何区别——区别只在「能不能被宿主当 entry 跑」。`@library` 这个语言级标记把这件事写到代码里，避免靠口头约定或目录结构来传递。

## `@library` 是什么、做什么

写在文件最外层字典前的根级别装饰器：

```relon
@library
{
    @schema User: { String name: * },
    greet(User u): "Hello, " + u.name
}
```

它的语义可以拆成三条：

1. **拒绝当 entry 跑**：宿主调用 `relon::value_from_str` / `value_from_file` / `json_from_*` 时，如果文件根节点带 `@library`，立即返回 `Error::LibraryAsEntry { name }`，**根本不进入求值**。
2. **被 import 完全不变**：`@import("./lib.relon", as="lib")` 或 `spread=true` 行为跟普通文件一模一样——`@library` 标记不影响导入语义。
3. **只看根节点**：嵌套字典里的 `@library` 是普通数据，不参与文件级语义。

## `@library` 不做什么

为了不让大家高估这个标记，列一下它**不**承担的职责：

- ❌ 不限制库文件里能放什么——你照样可以塞 schema、装饰器、闭包、常量。
- ❌ 不改变求值/类型规则——库被 import 后，按它在 import 点的语义跑。
- ❌ 不做 access control / 可见性——它不是 `pub`/`private` 关键字。
- ❌ 不做版本/兼容标记——那是包管理器的事。

把它当成一个「用法注解」即可：声明意图，并在边界做一次硬性 gate。

## 三场景对照

| 文件用途 | 加 `@library`？ | 行为 |
| --- | --- | --- |
| 平台共享库（schema、纯函数、共用常量） | ✅ 加 | 防止被错误地当 entry 跑；只能 `@import` |
| 业务入口（被宿主直接求值） | ❌ 不加 | 默认双用，宿主 `value_from_file` 拿 JSON |
| 一次性脚本 / fixture / demo | ❌ 不加 | 直接跑，立即看输出 |

第二行的「双用」是默认行为：不加标记的文件**既能**被 entry 化，**也能**被 import。多数 fixture 和小项目就走这条路。

## 库里能放什么

跟普通文件一样——`@library` 不限制内容形态。常见的几类：

- **`@schema` 定义**：业务领域的契约（`User`、`Order`、`Notification` 这些名词）。
- **装饰器配置**：`@ensure.*` 的预设、自定义 `@expect` 消息组合。
- **共享闭包**：`format_currency`、`normalize_phone` 这种纯函数。
- **基础常量**：`{ DEFAULT_TIMEZONE: "UTC", MAX_RETRIES: 3, ... }`。
- **嵌套库**：库里继续 `@import` 别的库——这跟普通文件一致。

## 完整例子：platform + business

**`platform/notify.relon`**——平台团队的库：

```relon
@library
{
    // 通知通道：sum-type tagged enum
    @schema Channel: Enum<
        Email { String address: *, String subject: * },
        SMS   { String phone: * },
        Push,
    >,

    // 业务通用的「带主体的通知」契约
    @schema Notification: {
        Channel via: *,
        String title: *,
        @default((self) => "[" + self.title + "]")
        String summary: *
    },

    // 帮助函数：根据通道渲染人类可读的描述
    describe(Notification n): n.via match {
        Email: f"邮件: ${n.via.address}",
        SMS:   f"短信: ${n.via.phone}",
        Push:  "推送"
    }
}
```

**`app/main.relon`**——业务团队的 entry，**不标** `@library`：

```relon
@import("../platform/notify.relon", spread=true)
{
    Notification welcome: {
        via: Channel.Email { address: "u@x.com", subject: "Hi" },
        title: "Welcome"
    },

    rendered: describe(welcome)
}
```

宿主端：

```rust
let json = relon::json_from_file("app/main.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

如果谁不小心把 `notify.relon` 当 entry 喂进去了，立刻被截住：

```text
Error::LibraryAsEntry { name: Some(".../platform/notify.relon") }
```

边界硬错误，不会有「跑了一半才发现没主入口」这种玄学问题。

## 何时该标 `@library`，何时不标

**该标**：

- 文件**只**为别人提供 schema/函数/常量，自己不打算被宿主跑。
- 平台团队的「公共词表」、第三方扩展包的入口文件。
- 多个业务 entry 共享同一份契约，避免任意一个 entry 都能反过来当库使。

**不标**：

- 业务 entry——它就是宿主求值的目标。
- CLI / 工具入口——直接跑直接出 JSON。
- 测试 fixture、demo——不必加心智负担。

**可标可不标**：

- 一些只在开发期被本人用的脚本——默认（不标）就够。

## 接下来

- 跨文件复用规则：[模块与作用域](./modules.md)
- Rust 端嵌入：[嵌入宿主](./host-integration.md)
- 用 schema 定义业务词表：[类型与契约](./types.md)
