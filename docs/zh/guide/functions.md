# 函数与闭包 (Functions & Closures)

在 Relon 中，函数是「一等公民」（First-class Citizens）。它们可以被保存在字典里、作为参数传递给其他函数，或者通过相对引用在其它地方调用。

Relon 默认启用严格静态分析。用户文档里的函数示例因此都写出参数和
返回类型；只有在快速草稿或 playground 实验时，才建议用 `#relaxed`
临时退出严格推断。

## 双轨语法 (Double-Track Syntax)

### 1. 方法简写 (Method Declarations)

如果你是在编写类似于「标准库」或者一个组件的业务逻辑，你很可能会
把函数放在字典的顶层。这时候使用方法简写会让结构非常清晰：

```relon
{
    // 推荐：把参数和返回值都写出来，strict 模式可以直接检查
    Int sum(Int a, Int b): a + b,

    Int multiply(Int a, Int b): a * b
}
```

方法简写本质上是定义了一个字典键值对。它比
`"sum": (Int a, Int b) -> Int => a + b` 更适合长文件：名字、参数、
返回类型和实现排在同一行，review 时一眼能看清契约。

### 2. 箭头函数 (Arrow Functions)

当你需要将一个快速的小逻辑传递给类似 `map` 或 `filter` 这样的高阶函数时，内联的箭头函数是最佳选择：

```relon
#import list from "std/list"
{
    List<Int> numbers: [1, 2, 3, 4, 5],

    List<Int> doubled:
        list.map(&sibling.numbers, (Int x) -> Int => x * 2),

    List<Int> evens:
        list.filter(&sibling.numbers, (Int x) -> Bool => x % 2 == 0)
}
```

## 管道运算符 (Pipe Operator)

在处理数据流时，嵌套的函数调用 `a(b(c(x)))` 往往难以阅读。Relon
支持使用 `|` 管道运算符将前一个表达式的求值结果，隐式地作为下一个
函数调用的第一个参数。

当前 analyzer 对「管道 + relaxed std 模块 wrapper」的返回类型推断还
比较保守；如果整条管道已经由上下文保证正确，可以在实验性脚本里用
`#relaxed`。发布给业务使用的文件，仍建议先写成显式调用并加返回类型，
等类型推断覆盖到这类组合后再改成管道风格。

```relon
#relaxed
#import string from "std/string"
{
    words: "apple,banana,cherry",

    // 普通函数嵌套
    count_normal: len(string.split(&sibling.words, ",")),

    // 管道运算符，语义自左向右流动
    count_piped: &sibling.words | string.split(",") | len()
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
    Int factorial(Int n): n <= 1 ? 1 : n * factorial(n - 1),

    // 调用它
    Int result: factorial(5) // 输出 120
}
```
