# 类型与契约 (Types & Schema)

在大型项目中，动态语言往往会因为缺乏契约而失控。Relon 通过引入名义类型系统（Nominal Types）和结构化契约，确保数据在经过复杂的动态推导和合并后，依然完美符合业务预期。

## 基础类型标记 (Type Hints)

你可以在几乎所有的标识符前添加类型标记。当类型标记存在时，Relon 引擎会在求值时执行严格的类型检查。如果检查失败，求值过程将抛出具体的错误。

```javascript
{
    // 基本类型标记
    String name: "Relon",
    Int port: 8080,
    
    // 可选类型标记（使用 ? 后缀）
    String? optional_desc: null,
    
    // 泛型标记
    List<Int> scores: [100, 95, 80],
    Dict<String, Bool> flags: { "active": true, "hidden": false }
}
```

内置的基础类型名称包括：`Any`, `Int`, `Float`, `Number` (兼容 Int 和 Float), `String`, `Bool`, `Null`, `List`, `Dict`, `Closure`。

## 枚举类型 (Enum)

`Enum` 是一个特殊的泛型结构，它要求被标记的值必须是其参数列表中的一种：

```javascript
{
    String theme: Enum<"light", "dark", "system">,
    
    // 枚举也可以针对其他类型进行约束
    id: Enum<Int, String>
}
```

## Schema 定义与契约守卫 (Identity Guards)

仅仅标记 `Dict` 有时是不够的，你往往需要验证字典内部长成了什么样子。这就是 `@schema` 装饰器登场的时候。

### 1. 定义 Schema

通过 `@schema` 定义的类型，其字段值将被视作**计算谓词 (Predicates)** 而非普通的数据。你可以使用 `*` 来代表“任意匹配”，或者使用一个闭包（函数）来进行自定义的业务校验。

```javascript
{
    @schema ButtonConfig: {
        // 类型必须是 String，内容任意匹配
        String type: *,
        
        // 自定义校验：宽度必须介于 10 到 100 之间
        @expect("Width must be between 10 and 100")
        Int width: (w) => w >= 10 && w <= 100,
        
        // 设置默认值
        @default(false) 
        Bool disabled: *
    }
}
```

### 2. 身份赋予 (Branding) 与名义类型

当一个普通的字典前面被标记了你定义的 Schema 类型时，魔法就发生了。

```javascript
{
    // 为这个匿名对象赋予 ButtonConfig 身份
    ButtonConfig my_btn: { type: "submit", width: 50 }
}
```

- **校验**：引擎将立即对 `my_btn` 运行 `ButtonConfig` 定义的契约。
- **默认值注入**：因为我们在 Schema 中声明了 `@default(false)`，所以即使 `my_btn` 没有写 `disabled`，最终求值出的字典中也会被自动注入 `disabled: false`。
- **身份守卫 (Identity Guard)**：`my_btn` 被盖上了 `ButtonConfig` 的“品牌烙印”。以后无论是谁尝试通过深合并 (`+` 算子或 `dict.merge`) 修改 `my_btn`，修改后的结果都会被**再次全量强制校验**！

```javascript
{
    // 💥 在合并发生时，程序会立即报错："Width must be between 10 and 100"
    // 从而避免非法的属性污染你的业务结构
    invalid_btn: &sibling.my_btn + { width: 999 } 
}
```

## Schema 混入与组合 (Mixins)

在组件库中，你常常需要通过基础配置扩展出高级配置。由于 Schema 也是头等值，你可以使用 `+` 直接将它们合并起来！

```javascript
{
    @schema BaseControl: { 
        String id: *,
        @default(false) Bool disabled: *
    },
    
    // 继承 BaseControl 的约束，并混入额外的属性
    @schema IconControl: &sibling.BaseControl + {
        String icon_path: *
    },
    
    // 拥有完整约束的最终实例
    IconControl final_btn: { id: "btn_1", icon_path: "/icons/save.png" }
}
```
通过以上机制，Relon 提供了一套从松散数据到严格契约的渐进式演进路径。
