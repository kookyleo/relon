# 基础语法

Relon 的语法极度贴近 JSON 和现代 JavaScript，它的设计目标是让你不需要查阅手册就能看懂大部分的配置代码。

## 数据类型 (Primitives)

Relon 原生支持 JSON 的所有基本类型，并在这个基础上做了一些增强：

```relon
{
    "string": "Hello, Relon!",
    "raw_string": r#"This is a raw string where \n is literal"#,
    "template": f"The number is {10 * 2}", // f-string 字符串插值

    "integer": 42,
    "float": 3.14159,
    "hex": 0xFF,
    "binary": 0b1010,

    "boolean": true,
    "null_value": null
}
```

## 集合类型 (Collections)

集合类型在 Relon 中是进行数据建模的核心。

### 列表 (List)

列表（数组）可以包含任意类型的元素。

```relon
[
    1,
    "two",
    { a: 3 }
]
```

#### 列表推导式 (List Comprehensions)

这是从 Python 借鉴而来的强大特性，非常适合动态生成数组：

```relon
[x * 2 for x in range(5) if x % 2 == 0]
// 最终求值结果为：[0, 4, 8]
```

### 字典 (Dict)

字典对应 JSON 中的 Object，它的键必须是字符串或能够转为字符串的表达式。

#### 动态键名 (Dynamic Pathing)

在方括号 `[]` 内部，你可以写入任意表达式来动态计算键名：

```relon
{
    prefix: "user_",
    id: 42,
    // 使用动态键名拼接
    [&sibling.prefix + &sibling.id]: "Alice"
}
```

#### 展开运算符 (Spread Operator)

你可以使用 `...` 语法将另一个列表或字典展开合并到当前集合中。如果在字典中展开，后出现的键会覆盖之前的同名键。

```relon
{
    base: { host: "localhost", port: 80 },
    prod: {
        ...&sibling.base,
        port: 443 // 覆盖了 base 中的 port
    }
}
```

## 文档根 (Document Root)

每个 `.relon` 文件的**根必须是字典或列表**——也就是说，整份文档必须以 `{` 或 `[` 开头（前面允许有装饰器、注释、空白）。裸标量（如 `42`、`true`、`"hello"`）作为整份文档的根**不被接受**，即便这些形式在现行 JSON 规范（RFC 8259）中是合法的。

```relon
// 合法
{ "value": 42 }

// 合法
[1, 2, 3]

// 合法：顶层可以挂装饰器
@import("std/string", as="string")
{ "shouted": string.upper("hi") }

// 不合法：根不能是标量
42
```

这是 Relon 的有意设计：`@import`、`&root` 引用、字段私有约定、闭包过滤等核心特性都建立在「根是命名容器」之上。如果你确实需要把整份配置表达成单一标量，请用 `{ "value": 42 }` 或 `[42]` 包一层。

## 字段可见性 — `@private`

由于配置通常需要对外导出为纯净的 JSON，内部的逻辑需要被隐藏。Relon
通过 `@private` 装饰器显式声明字段不对外可见：

```relon
{
    @private
    helper(v): "<" + v + ">",
    display: helper("hi")
}
// JSON 输出：{ "display": "<hi>" }   // helper 被隐藏
```

`@private` 字段：

- 仍然存活在所属 dict 的局部作用域里，**同一个 dict 的其它字段可以
  引用它**（上例中 `display` 调用 `helper` 即正常）。
- **不会写入** dict 的导出 map：所以
  - 不会出现在 JSON 输出中；
  - 跨 dict 的 `&root` / `&sibling` 引用看不到它；
  - 通过 `@import(..., as="lib")` 后访问 `lib.private_field` 会以
    `VariableNotFound` 失败；
  - 通过 `@import(..., spread=true)` 时也不会被复制进当前作用域。

字典中如果值是一个**闭包（函数）**，默认 JSON 投影器会自动过滤掉它
——这是 `@private` 之外的另一道防线，专门处理「Value 没有 JSON 表
示」的情况。

> 历史说明：早期版本曾用 `_` 前缀做隐式约定。该约定已**完全取消**：
> 标识符仍然可以以 `_` 开头（例如内部 intrinsic `_list_map`），但
> 它不再代表任何可见性、导入或投影行为。需要私有性请使用
> `@private`。

## 算术与逻辑运算符

Relon 支持标准的算术 (`+`, `-`, `*`, `/`, `%`) 和逻辑比较运算符 (`>`, `<`, `>=`, `<=`, `==`, `!=`, `&&`, `||`)。另外，你还可以使用三元运算符处理条件：

```relon
{
    "status": status_code == 200 ? "OK" : "Error"
}
```
