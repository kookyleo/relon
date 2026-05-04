# 模块与作用域 (Modules & Scopes)

如果配置变得庞大，我们自然需要将其切分到多个文件中。Relon 提供了基于 `@import` 装饰器的模块化系统。由于 Relon 是一门没有全局变量的声明式语言，模块就是组织复用逻辑的最佳边界。

## 导入 (Importing)

在字典或文件的顶级作用域，你可以使用 `@import` 来引入其他的 `.relon` 文件。

### 命名空间导入

这是最常见也最安全的做法。引擎会求值目标文件，并将其暴露为一个绑定在你指定名称上的“模块对象”。

```javascript
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

如果你有一堆通用的 Schema 或者常用的纯函数，每次都通过命名空间来调用可能会显得累赘。这时可以启用 `spread=true`，它会将目标文件顶层抛出的所有变量“解构”合并进当前作用域。

```javascript
@import("./helpers.relon", spread=true)
{
    // 如果 helpers.relon 导出了 shout 函数，你可以直接在这里使用
    msg: shout("hello")
}
```

#### 导入保护 (Import Protection)

需要注意的是，如果在平铺导入时发生了名称冲突，**覆盖行为会发生**。为了保护命名空间，Relon 遵循一条非常硬性的约定：
**目标模块中，所有以 `_` 下划线开头的字段都会被平铺导入自动跳过！**

```javascript
// helpers.relon
{
    // 将被平铺导入
    shout(v): v + "!!!",
    
    // 私有助手函数，平铺导入时自动隐藏
    _add(a, b): a + b 
}
```

## 相对引用 (Relative References)

当你没有进行跨文件的模块调用时，你可以在单文件内的深度嵌套对象中使用“亲属引用”来访问周围的数据。

Relon 支持以下的定位符：
- `&root`: 永远指向当前文件解析树的最顶层字典。
- `&sibling`: 当前同级目录。
- `&uncle`: 上一级目录（父级的兄弟）。

在列表处理中，你可以使用以下基于游标的相对引用：
- `&prev`: 获取列表中的上一个元素（如果是第一个元素则返回 null）。
- `&next`: 获取列表中的下一个元素（支持前瞻预测/Lookahead）。
- `&index`: 获取当前元素在列表中的索引（整数）。
- `&this`: 获取列表或整个遍历范围的顶层上下文容器。

```javascript
{
    steps: [
        { title: "步骤 1", done: true,  next_ready: &next.done },
        { title: "步骤 2", done: false, index: &index }
    ]
}
```
通过相对引用配合 `&prev` 和 `&next`，Relon 在处理状态机配置和复杂列表流程时显得游刃有余，这一切都受益于其底层的全量懒求值模型（Thunks）。
