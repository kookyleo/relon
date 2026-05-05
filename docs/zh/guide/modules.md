# 模块与作用域 (Modules & Scopes)

如果配置变得庞大，我们自然需要将其切分到多个文件中。Relon 提供了基于 `@import` 装饰器的模块化系统。由于 Relon 是一门没有全局变量的声明式语言，模块就是组织复用逻辑的最佳边界。

## 导入 (Importing)

在字典或文件的顶级作用域，你可以使用 `@import` 来引入其他的 `.relon` 文件。

### 命名空间导入

这是最常见也最安全的做法。引擎会求值目标文件，并将其暴露为一个绑定在你指定名称上的「模块对象」。

```relon
// main.relon
@import("./lib.relon", as="theme")
{
    // 调用 theme 模块内定义的工具函数或颜色变量
    button_color: theme.colors.primary,

    // 或者引用其中的 Schema
    theme.ButtonConfig my_button: { label: "Click" }
}
```

### 平铺导入 (Spread Import)

如果你有一堆通用的 Schema 或者常用的纯函数，每次都通过命名空间来调用可能会显得累赘。这时可以启用 `spread=true`，它会将目标文件顶层抛出的所有变量「解构」合并进当前作用域。

```relon
@import("./helpers.relon", spread=true)
{
    // 如果 helpers.relon 导出了 shout 函数，你可以直接在这里使用
    msg: shout("hello")
}
```

#### 导入保护 (Import Protection)

需要注意的是，如果在平铺导入时发生了名称冲突，**覆盖行为会发生**。为了保护命名空间，Relon 遵循一条非常硬性的约定：
**目标模块中，所有以 `_` 下划线开头的字段都会被平铺导入自动跳过！**

```relon
// helpers.relon
{
    // 将被平铺导入
    shout(v): v + "!!!",

    // 私有助手函数，平铺导入时自动隐藏
    _add(a, b): a + b
}
```

## `@library` 库标记

`@import` 解决了「拆文件」的问题，`@library` 解决的是「这个文件该不该被宿主当 entry 跑」的问题。

### 它是什么

`@library` 是一个**根级别**装饰器，写在文件最外层的字典前面：

```relon
@library
{
    @schema User: { String name: * },
    greet(User u): "Hello, " + u.name
}
```

效果：

- 这个文件**不能**作为 host entry 被求值（`relon::value_from_str` / `value_from_file` 等会直接返回 `Error::LibraryAsEntry`）。
- 这个文件**仍然**可以被其他文件 `@import`——`@import` 完全不在意目标是否标了 `@library`。
- 嵌套字典里的 `@library` 是普通数据，不算数；只有根节点上的才生效。

### 不标的文件——双用

不加 `@library` 标记的文件是**双用**的：

- 既能被宿主直接求值（拿到 JSON 输出）；
- 也能被 `@import` 进来当模块用。

这是默认行为；新手不需要知道 `@library` 的存在也能正常工作。

### 三个场景对照

| 场景 | 文件加 `@library`？ | 行为 |
| --- | --- | --- |
| 平台团队的共享库（schema、纯函数、装饰器配置） | ✅ 加 | 防止被错误地当 entry 跑；只能 `@import` |
| 业务团队的应用入口（被宿主求值） | ❌ 不加 | 默认双用，宿主调用 `value_from_file` 拿 JSON |
| 开发期的一次性 demo / fixture | ❌ 不加 | 直接跑、直接看输出 |

### 一个完整的 platform/business 配对

**`platform/notify.relon`**（库，标 `@library`）：

```relon
@library
{
    @schema Channel: Enum<
        Email { String address: *, String subject: * },
        SMS   { String phone: * },
        Push,
    >,

    @schema Notification: {
        Channel via: *,
        String title: *
    }
}
```

**`app/main.relon`**（业务 entry，**不标** `@library`）：

```relon
@import("../platform/notify.relon", spread=true)
{
    Notification welcome: {
        via: Channel.Email { address: "u@x.com", subject: "Hi" },
        title: "Welcome"
    }
}
```

宿主直接 `relon::json_from_file("app/main.relon")` 拿到 JSON。如果有人不小心把 `notify.relon` 当 entry 喂进来：

```text
Error: LibraryAsEntry { name: Some("...") }
```

错误立即在边界被截住，根本不会进入求值流程。

### 何时该标、何时不标

- **该标**：你的文件**只**为别人提供 schema/函数/常量，自己不打算被宿主跑——比如平台团队的标准库、共享业务术语库、第三方扩展包的入口。
- **不标**：你的文件**会**被宿主当 entry 跑——业务应用、CLI 入口、生成 JSON 的脚本。
- **可标可不标**：纯粹给开发自己用的本地脚本——不必纠结，默认（不标）就够。

## 相对引用 (Relative References)

当你没有进行跨文件的模块调用时，你可以在单文件内的深度嵌套对象中使用「亲属引用」来访问周围的数据。

Relon 支持以下的定位符：
- `&root`: 永远指向当前文件解析树的最顶层字典。
- `&sibling`: 当前同级目录。
- `&uncle`: 上一级目录（父级的兄弟）。

在列表处理中，你可以使用以下基于游标的相对引用：
- `&prev`: 获取列表中的上一个元素（如果是第一个元素则返回 null）。
- `&next`: 获取列表中的下一个元素（支持前瞻预测/Lookahead）。
- `&index`: 获取当前元素在列表中的索引（整数）。
- `&this`: 获取列表或整个遍历范围的顶层上下文容器。

```relon
{
    steps: [
        { title: "步骤 1", done: true,  next_ready: &next.done },
        { title: "步骤 2", done: false, index: &index }
    ]
}
```

通过相对引用配合 `&prev` 和 `&next`，Relon 在处理状态机配置和复杂列表流程时显得游刃有余，这一切都受益于其底层的全量懒求值模型（Thunks）。
