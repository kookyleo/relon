# 简介：什么是 Relon？

Relon 是一个专为现代 Web 框架（如 UI 组件库）和复杂的工业级配置系统而设计的**可编程配置语言**。

你可以把 Relon 看作是具有完整表达式能力的超集 JSON，它具备了像 TypeScript 那样完备的类型推断与结构契约机制，但剥离了一切复杂的副作用与语句控制流。

## 设计哲学 (Philosophy)

- **一切皆为值 (Everything is a Value)**: Relon 中没有语句（statements），没有 `return` 关键字。整个文件及其内部的每一个闭包、列表推导式、解构操作，其结果都是一个纯粹的、可被 JSON 序列化的数据。
- **配置即函数 (Config as Code)**: 支持使用现代的箭头函数 (Arrow Functions) 和对象方法简写来组织代码的动态复用。
- **名义类型与契约守卫 (Nominal Types & Identity Guards)**: 通过 `@schema` 装饰器定义不可篡改的类型契约。这并非仅为了静态分析，而是参与到运行时的核心：它会在执行层拦截非法的动态结构组合。

## 为什么不直接使用 JSON 或 YAML？

虽然 JSON 和 YAML 非常流行，但在面对大规模微前端框架、复杂的 CI/CD 流水线以及深层组件嵌套时，它们显得力不从心。你可能经常需要用到：
- 数据之间的互相引用（例如 `color: &root.theme.primary`）
- 默认值的合并和继承
- 配置数据的动态生成（如使用 List Comprehension 渲染选项）
- 对外输出前的结构校验

Relon 正是诞生于解决这些痛点。它天然提供了相对引用（Sibling, Uncle, Root）、强大的展开运算符（Spread `...`），以及丰富的内置操作符。

## 快速浏览

以下是一个典型的 Relon 结构：

```javascript
{
    // 1. 定义数据契约
    @schema ButtonConfig: {
        String variant: Enum<"primary", "secondary", "ghost">,
        @default(false) Bool disabled: *,
        String label: (s) => string.len(s) > 0
    },

    // 2. 基础配置库
    base_theme: {
        primary_color: "#1890ff",
        spacing: 8
    },

    // 3. 利用动态引用和混入实现业务组件
    ButtonConfig submit_btn: {
        variant: "primary",
        label: "提交",
        // 相对引用：获取同级兄弟节点的颜色值
        _color: &sibling.base_theme.primary_color
    }
}
```

准备好深入了解了吗？请前往下一章 [基础语法](./syntax.md) 开始你的旅程。
