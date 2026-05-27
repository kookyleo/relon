# 模块与作用域 (Modules & Scopes)

如果配置变得庞大，我们自然需要将其切分到多个文件中。Relon 提供了基于 `#import` 指令的模块化系统。由于 Relon 是一门没有全局变量的声明式语言，模块就是组织复用逻辑的最佳边界。

## 导入 (Importing)

在字典或文件的顶级作用域，你可以使用 `#import` 来引入其他的 `.relon` 文件。统一的语法：

```text
#import <bindspec> from "<path>"
```

`<bindspec>` 有三种形态：

| 形态 | 写法 | 含义 |
| --- | --- | --- |
| 命名空间 | `lib` | 将整个模块绑定到名字 `lib` |
| 平铺 | `*` | 把模块导出的全部字段并入当前作用域 |
| 析构 | `{ a, b as c }` | 只取 `a`、`b`，并把 `b` 重命名为 `c` |

### 命名空间导入

这是最常见也最安全的做法。引擎会求值目标文件，并将其暴露为一个绑定在你指定名称上的「模块对象」。

```relon
// main.relon
#import theme from "./lib.relon",
{
    // 调用 theme 模块内定义的工具函数或颜色变量
    button_color: theme.colors.primary,

    // 或者引用其中的 Schema
    theme.ButtonConfig my_button: { label: "Click" }
}
```

### 平铺导入 (Spread Import)

如果你有一堆通用的 Schema 或者常用的纯函数，每次都通过命名空间来调用可能会显得累赘。这时可以用 `*`，它会将目标文件顶层抛出的所有变量「解构」合并进当前作用域。

```relon
#import * from "./helpers.relon",
{
    // 如果 helpers.relon 导出了 shout 函数，你可以直接在这里使用
    msg: shout("hello")
}
```

### 析构导入 (Destructuring Import)

只想引入若干个名字、可能伴随重命名时使用：

```relon
#import { upper, lower as lo } from "std/string",
{
    a: upper("hello"),
    b: lo("WORLD")
}
```

#### 导入保护 (Import Protection)

如果在平铺导入时发生了名称冲突，**覆盖行为会发生**。为了保护命名空
间，把不希望被平铺导入的字段标上 `#internal` 指令：私有字段不会写
入模块的导出 map，所以平铺导入会自然跳过它们，命名空间形式也访问
不到（`lib.private_field` → `VariableNotFound`）。

```relon
// helpers.relon
{
    // 将被平铺导入
    shout(v): v + "!!!",

    // 私有助手函数：不会被任何 #import 形式带出去
    #internal
    add(a, b): a + b
}
```

> 历史说明：早期版本用 `_` 前缀做隐式约定，并使用 `@private` 装
> 饰器形式。两者都已**完全取消**，请改用 `#internal` 指令。详见
> [`syntax.md`](./syntax.md#字段可见性-internal)。

## 入口程序与库

Relon 没有「文件级别 library/entry 标记」这一概念。是否有 `#main(...)`
签名决定了文件**怎么用**：

- 文件**有** `#main(...)`：是入口程序。宿主必须通过
  `Evaluator::run_main(scope, args)` 推入参数才能跑出结果。直接当
  库 `#import` 也允许（参数不会被使用，只取它的导出）。
- 文件**没有** `#main(...)`：是「无契约」的纯数据 / 库文件。既可以
  被 `#import` 当模块用，也可以被宿主直接 `eval_root` 求值得到一份
  纯 JSON。

完整示例：

```relon
// app/main.relon —— 入口程序
#import * from "../platform/notify.relon",
#main(Notification notice)
{
    delivered: notice.title + " (via " + notice.via + ")"
}
```

```relon
// platform/notify.relon —— 共享库（无 #main）
{
    #schema Channel Enum<
        Email { String address: *, String subject: * },
        SMS   { String phone: * },
        Push
    >,
    #schema Notification {
        Channel via: *,
        String title: *
    }
}
```

宿主侧：

```rust
let mut args = HashMap::new();
args.insert(
    "notice".to_string(),
    /* host 推入的 Value::Dict */ notice_value,
);
let result = evaluator.run_main(&scope, args)?;
```

宿主试图把入口程序当库直接 `eval_root`，会得到 `NoMainSignature` 错
误——错误立即在边界截住，绝不会进入求值流程。反之，库文件没有
`#main` 时调用 `run_main` 也是同样的错误。

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
