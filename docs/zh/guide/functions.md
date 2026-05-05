# 函数与闭包 (Functions & Closures)

在 Relon 中，函数是「一等公民」（First-class Citizens）。它们可以被保存在字典里、作为参数传递给其他函数，或者通过相对引用在其它地方调用。

为了满足不同场景的表达需求，Relon 提供了两种函数语法。

## 双轨语法 (Double-Track Syntax)

### 1. 方法简写 (Method Declarations)

如果你是在编写类似于「标准库」或者一个组件的业务逻辑，你很可能会把它们放在字典的顶层。这时候使用方法简写会让结构非常清晰：

```relon
{
    // 无类型标记
    sum(a, b): a + b,

    // 带类型标记（推荐）
    Int multiply(Int a, Int b): a * b
}
```
*提示：方法简写本质上是定义了一个字典的键值对，这在编写长文件时比传统的 `"sum": (a, b) => a + b` 更具可读性。*

### 2. 箭头函数 (Arrow Functions)

当你需要将一个快速的小逻辑传递给类似 `map` 或 `filter` 这样的高阶函数时，内联的箭头函数是最佳选择：

```relon
{
    numbers: [1, 2, 3, 4, 5],
    // 使用标准库的高阶函数
    doubled: list.map(&sibling.numbers, (x) => x * 2),

    // 带类型标记的箭头函数
    evens: list.filter(&sibling.numbers, (Int x) -> Bool => x % 2 == 0)
}
```

## 管道运算符 (Pipe Operator)

在处理数据流时，嵌套的函数调用 `a(b(c(x)))` 往往难以阅读。Relon 支持使用 `|` 管道运算符将前一个表达式的求值结果，隐式地作为第一个参数传递给下一个函数调用。

```relon
{
    words: "apple,banana,cherry",

    // 使用普通的函数嵌套
    count_normal: list.len(string.split(&sibling.words, ",")),

    // 使用管道运算符，语义自左向右流动
    count_piped: &sibling.words | string.split(",") | list.len()
}
```

## Where 绑定块

有时你不想提取一个函数，只是想在一个复杂的长表达式中避免重复计算某个值。这时候可以使用 `where` 子句。

`where` 允许你为一个单一的表达式绑定临时的局部作用域变量。

```relon
{
    volume: (width * height * depth) where {
        width: 10,
        height: 20,
        depth: 5
    }
}
```

## 递归调用 (Recursion)

由于字典内的键是可以在当前作用域被相互引用的（或者你直接利用 `&sibling`），你完全可以在 Relon 中写出递归闭包：

```relon
{
    // 定义一个求阶乘的函数
    factorial(n): n <= 1 ? 1 : n * factorial(n - 1),

    // 调用它
    result: factorial(5) // 输出 120
}
```
