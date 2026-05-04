# 基础语法

Relon 的语法极度贴近 JSON 和现代 JavaScript，它的设计目标是让你不需要查阅手册就能看懂大部分的配置代码。

## 数据类型 (Primitives)

Relon 原生支持 JSON 的所有基本类型，并在这个基础上做了一些增强：

```javascript
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

```javascript
[
    1, 
    "two", 
    { a: 3 }
]
```

#### 列表推导式 (List Comprehensions)

这是从 Python 借鉴而来的强大特性，非常适合动态生成数组：

```javascript
[x * 2 for x in range(5) if x % 2 == 0]
// 最终求值结果为：[0, 4, 8]
```

### 字典 (Dict)

字典对应 JSON 中的 Object，它的键必须是字符串或能够转为字符串的表达式。

#### 动态键名 (Dynamic Pathing)

在方括号 `[]` 内部，你可以写入任意表达式来动态计算键名：

```javascript
{
    prefix: "user_",
    id: 42,
    // 使用动态键名拼接
    [&sibling.prefix + &sibling.id]: "Alice"
}
```

#### 展开运算符 (Spread Operator)

你可以使用 `...` 语法将另一个列表或字典展开合并到当前集合中。如果在字典中展开，后出现的键会覆盖之前的同名键。

```javascript
{
    base: { host: "localhost", port: 80 },
    prod: {
        ...&sibling.base,
        port: 443 // 覆盖了 base 中的 port
    }
}
```

## 下划线命名法则 (The Underline Convention)

由于配置通常需要对外导出为纯净的 JSON，内部的逻辑需要被隐藏。
- 字典中，如果值是一个**闭包（函数）**，它在最终的 JSON 序列化阶段会被自动过滤丢弃。
- **推荐风格**：任何以 `_` 开头的键通常代表“内部状态”或“私有变量”。
- **导入保护**：在使用 `@import("path", spread=true)` 平铺导入外部模块时，带 `_` 的字段将被自动跳过，从而避免污染当前的命名空间。

## 算术与逻辑运算符

Relon 支持标准的算术 (`+`, `-`, `*`, `/`, `%`) 和逻辑比较运算符 (`>`, `<`, `>=`, `<=`, `==`, `!=`, `&&`, `||`)。另外，你还可以使用三元运算符处理条件：

```javascript
{
    "status": status_code == 200 ? "OK" : "Error"
}
```
