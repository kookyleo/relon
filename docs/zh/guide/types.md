# 类型与契约 (Types & Schema)

在大型项目中，动态语言往往会因为缺乏契约而失控。Relon 通过引入名义类型系统（Nominal Types）和结构化契约，确保数据在经过复杂的动态推导和合并后，依然完美符合业务预期。

## 基础类型标记 (Type Hints)

你可以在几乎所有的标识符前添加类型标记。当类型标记存在时，Relon 引擎会在求值时执行严格的类型检查。如果检查失败，求值过程将抛出具体的错误。

```relon
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

内置的基础类型名称包括：`Any`, `Int`, `Float`, `Number`（兼容 Int 和 Float）, `String`, `Bool`, `Null`, `List`, `Dict`, `Closure`。

## 联合类型 / Untagged Enum

`Enum<...>` 在 Relon 里有两种形态：本节先讲**无标签联合类型**，下一节讲**带标签的 sum type**。

无标签联合类型用来约束一个值「必须是参数列表中的某一个」。参数可以是字面量，也可以是类型名：

```relon
{
    String theme: Enum<"light", "dark", "system">,

    // 类型集合的并集
    id: Enum<Int, String>
}
```

> 这种形式不会引入「品牌」或运行时标签，纯粹是约束。如果你的领域里有明确的「这是哪一种」语义（譬如「这条通知是 Email 还是 SMS」），那应该用下一节的 sum type。

## Sum types：带标签的枚举变体

当你想表达「这个值是若干互斥变体中的某一个，每种变体带不同字段」时——比如订单状态、通知通道、UI 节点——这就是 sum type 登场的地方。

### 在 schema 里声明

```relon
@schema Notification: Enum<
    Email { String address: *, String subject: * },
    SMS   { String phone: * },
    Push,
>
```

注意：

- `Email { ... }` 是带字段的变体，花括号里的语法跟普通 `@schema` 字段一样（类型标注 + 谓词）。
- `Push` 是「单元变体」（没有字段），声明时**不带花括号**。
- 各变体之间的字段彼此独立，不会自动合并。

### 构造一个变体

```relon
{
    a: Notification.Email { address: "x@y.z", subject: "hi" },
    b: Notification.SMS   { phone: "+1-555-0100" },

    // 单元变体在「构造」时仍然要写 `{}`——空大括号
    c: Notification.Push  {}
}
```

> 之所以构造时要写 `{}`，是为了让语法「这是一个值」始终一致：变体永远是 `EnumName.Variant { ... }` 三件套。

### 内存形态 vs JSON 输出

Relon 在内存里把变体存成一个普通的 dict，并附带两个隐含标签：`brand`（变体名，比如 `"Email"`）和 `variant_of`（所属枚举名，比如 `"Notification"`）。访问字段是**扁平的**：

```relon
{
    msg: Notification.Email { address: "x@y.z", subject: "hi" },
    addr: msg.address      // -> "x@y.z"，没有 .Email. 这一层
}
```

但 JSON 输出走**外部标签（externally tagged）**形式，把变体名作为外层 key：

```json
{
  "msg": { "Email": { "address": "x@y.z", "subject": "hi" } }
}
```

这是 Relon 唯一一种「打 brand 后输出形状会变」的情况——普通 `@schema User` 标过的 dict 输出仍然是扁平的，brand 只在运行时生效。

> 想换一种 sum-type 编码风格（内部标签、对象聚合等等）？通过 `Projector` trait 在宿主端定制，详见 [嵌入宿主](./host-integration.md)。

### 用 `match` 分发 + 编译期穷尽性

变体的常见用法是 `match`：

```relon
{
    msg: Notification.Email { address: "x@y.z", subject: "hi" },
    rendered: msg match {
        Email: f"emailed ${msg.address}",
        SMS:   f"texted ${msg.phone}",
        Push:  "pushed"
    }
}
```

当分析器**能静态推断**被匹配值的枚举类型时（例如 `msg` 是 `Notification` 类型字段，或者它本身是个 `VariantCtor`），它会把以下情况升级为**编译期 Error**：

| 诊断 | 触发条件 |
| --- | --- |
| `NonExhaustiveMatch` | 缺少变体且没有 `*` 通配符 |
| `UnknownVariant` | 写了不存在的变体名（带 did-you-mean） |
| `DuplicateMatchArm` | 同一个变体名出现了两次 |
| `HeterogeneousEnum` | `Enum<...>` 里同时混了字面量/类型并列项和命名变体 |

如果分析器无法推断（例如来自动态计算的值），这些检查退化为运行时；想要不写穷尽分支，加一条 `*: ...` 通配符即可。

### 一个完整的状态机例子

```relon
@library
{
    @schema Order: Enum<
        Pending  { String customer: * },
        Shipped  { String tracking: * },
        Delivered { String signed_by: * },
    >,

    // 把变体翻译成给前端用的人类可读字符串
    summarize(Order o): o match {
        Pending:   f"待发货：${o.customer}",
        Shipped:   f"在途：${o.tracking}",
        Delivered: f"已签收：${o.signed_by}"
    }
}
```

## Schema 定义与契约守卫 (Identity Guards)

仅仅标记 `Dict` 有时是不够的，你往往需要验证字典内部长成了什么样子。这就是 `@schema` 装饰器登场的时候。

### 1. 定义 Schema

通过 `@schema` 定义的类型，其字段值将被视作**计算谓词 (Predicates)** 而非普通的数据。你可以使用 `*` 来代表「任意匹配」，或者使用一个闭包（函数）来进行自定义的业务校验。

```relon
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

```relon
{
    // 为这个匿名对象赋予 ButtonConfig 身份
    ButtonConfig my_btn: { type: "submit", width: 50 }
}
```

- **校验**：引擎将立即对 `my_btn` 运行 `ButtonConfig` 定义的契约。
- **默认值注入**：因为我们在 Schema 中声明了 `@default(false)`，所以即使 `my_btn` 没有写 `disabled`，最终求值出的字典中也会被自动注入 `disabled: false`。
- **身份守卫 (Identity Guard)**：`my_btn` 被盖上了 `ButtonConfig` 的「品牌烙印」。以后无论是谁尝试通过深合并（`+` 算子或 `dict.merge`）修改 `my_btn`，修改后的结果都会被**再次全量强制校验**！

```relon
{
    // 在合并发生时，程序会立即报错："Width must be between 10 and 100"
    // 从而避免非法的属性污染你的业务结构
    invalid_btn: &sibling.my_btn + { width: 999 }
}
```

## Schema 混入与组合 (Mixins)

在组件库中，你常常需要通过基础配置扩展出高级配置。由于 Schema 也是头等值，你可以使用 `+` 直接将它们合并起来！

```relon
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

## 递归 schema

Schema 可以引用自身——这对菜单树、文件目录、AST 一类的递归结构很自然：

```relon
{
    @schema Menu: {
        String title: *,
        List<Menu>? children: *
    },

    Menu root: {
        title: "Home",
        children: [
            { title: "Products", children: [] },
            { title: "About" }
        ]
    }
}
```

> 实现层面递归校验深度上限为 20 层，远超绝大多数业务嵌套——如果你撞上了，多半是数据形状出问题，先反思 schema。

## 自定义校验消息（@expect）

默认情况下，校验失败抛出的错误消息是引擎根据谓词字符串拼出来的，可读性一般。`@expect("...")` 让你显式指定一条业务可读的消息：

```relon
{
    @schema Server: {
        @expect("Port must be between 0 and 65535")
        Int port: (p) => p > 0 && p < 65536
    },

    Server s: { port: 70000 }
    // → TypeMismatch { expected: "Port must be between 0 and 65535", ... }
}
```

`@expect` 一定跟一个谓词闭包搭配使用——给 `*` 加 `@expect` 是没意义的。

## 必填、可选、默认值、计算默认值

Relon 的 schema 字段把这四种语义放在同一个声明里，靠**修饰符的组合**区分：

```relon
{
    @schema User: {
        // 1. 必填（默认行为）：缺失即抛错
        String name: *,

        // 2. 可选（? 后缀）：缺失等价于 null
        String? bio: *,

        // 3. 字面量默认值（@default(value)）：缺失时填这个常量
        @default("user")
        String role: *,

        // 4. 计算默认值（@default((self) => ...)）
        // 当字段缺失时调用闭包，self 是「已知字段已经填好的实例视图」
        @default((self) => self.name + " <unset>")
        String display_name: *
    },

    // 用法
    User u: { name: "Ada" }
    // u.bio          == null
    // u.role         == "user"
    // u.display_name == "Ada <unset>"
}
```

几条务必注意的细节：

- 显式写出的字段值**永远赢过**默认值——无论字面量还是计算式。
- 计算默认值是**惰性触发**的：只在字段确实缺失时才求值，不会白白调用。
- 计算默认值的 `self` 看得到「显式写出的字段 + 已经被字面量默认值填上的字段」，但**看不到其他计算默认字段**——它们之间不会互相观察，避免出现求值循环。

## 接下来

- 把 schema 和帮助函数封装成可复用的库：[模块与作用域](./modules.md) / [库与入口](./library-vs-entry.md)
- 在 Rust 端注册自己的 `@schema` 用得着的原生函数：[嵌入宿主](./host-integration.md)
- 跑不可信脚本时把 schema 与 `@expect` 当作第一道防线：[沙箱与权限](./sandbox.md)
