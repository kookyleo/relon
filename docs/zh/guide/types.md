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
    String? optional_desc: None,

    // 泛型标记
    List<Int> scores: [100, 95, 80],
    Dict<String, Bool> flags: { "active": true, "hidden": false }
}
```

内置类型名称包括：`Int`, `Float`, `Number`（兼容 Int 和 Float）, `String`, `Bool`, `Option<T>`, `Result<T, E>`, `List<T>`, `Tuple<T1, T2, ...>`, `Dict<K, V>`, `Closure<...>`。Relon 没有 `null` 值；没有值用 `None`，只在导出 JSON 时投影为 JSON `null`。`Any` 在 v1.6 起从用户面退出（`ExplicitAnyForbidden`）；`List` / `Dict` / `Closure` / `Fn` 在 v1.7 起必须带泛型参数，bare 形式会被 `BareGenericContainer` 拒绝。Enum 定义使用下面的 `#enum` 写法。

## Enum：Rust-like 带标签枚举

Relon 的公开 enum 写法是 Rust-like 的 `#enum`。它表达「一个值属于若干互斥变体之一」，每个变体可以没有 payload、带命名字段，或带 tuple payload。

```relon
#enum Notification {
    Email { address: String, subject: String },
    SMS { phone: String },
    Push
}

#enum Packet {
    Pair(Int, String),
    Empty
}
```

Relon 不支持 `"up" | "down"` 这类字符串字面量 enum，也不支持 `#enum Stat { "up", "down" }`。如果只是想从外部字符串输入状态，见下方的 typed JSON string 输入规则。

### 构造变体

```relon
{
    a: Notification.Email { address: "x@y.z", subject: "hi" },
    b: Notification.SMS { phone: "+1-555-0100" },
    c: Notification.Push,

    pair: Packet.Pair(7, "x"),
    empty: Packet.Empty
}
```

规则和 Rust 对齐：

- unit variant 直接写 `EnumName.Variant`；不需要空 `{}`。
- struct variant 写 `EnumName.Variant { field: value }`。
- tuple variant 写 `EnumName.Variant(value1, value2)`。

match arm 可以解构 payload：

```relon
#main(Packet p) -> Int
p match {
    Pair(n, *): n + 1,
    Empty: 0
}
```

### 内存形态与 JSON 输出

Relon 内部把 enum value 表示为带标签的 `Value::Dict`：`brand` 是变体名，`variant_of` 是 enum 名。字段访问是扁平的：

```relon
{
    msg: Notification.Email { address: "x@y.z", subject: "hi" },
    addr: msg.address      // -> "x@y.z"，没有 .Email 这一层
}
```

默认 JSON 输出采用外部标签形式：

```json
{
  "msg": { "Email": { "address": "x@y.z", "subject": "hi" } },
  "pair": { "Pair": [7, "x"] },
  "empty": { "Empty": {} }
}
```

List 和 tuple 都输出为 JSON array；enum tuple variant 的 payload 也输出为 JSON array。

variant list 可以直接返回、用字面量构造，也可以在外围类型已知时由 `map`、`filter` 或 comprehension 产出：

```relon
#enum Stat { Up, Down }
#main(List<Int> xs) -> List<Stat>
xs.map((Int x) => x > 0 ? Stat.Up : Stat.Down)
```

同样适用于 `List<Option<T>>` 和 `List<Result<T, E>>`。

### 输入：typed JSON 进入 variant

CLI 和 WASM playground 的 `#main(args)` JSON 输入会读取入口签名。目标类型是 enum 时，JSON string 可以进入同名 unit variant：

```relon
#enum Stat { Up, Down }
#main(Stat s) -> Stat
s
```

传入：

```json
{ "s": "Up" }
```

输出：

```json
{ "Up": {} }
```

裸字符串只适用于 unit variant。带 payload 的 variant 使用和 JSON 输出一致的外部标签形态：

```relon
#enum Msg { Email { address: String }, Pair(Int, String), Push }
#main(Msg m) -> Msg
m
```

```json
{ "m": { "Email": { "address": "x@y.z" } } }
```

tuple variant 使用数组：

```json
{ "m": { "Pair": [7, "x"] } }
```

内建 `Option` 和 `Result` 也走目标类型感知边界。对
`#main(Option<Int> x)`，输入可以是 `null`、直接 payload `41`，也可以是外部标签
`{ "x": { "Some": { "value": 41 } } }`。`Int?` 遵循同一规则。对
`#main(Result<Int, String> r)`，使用 `{ "r": { "Ok": { "value": 41 } } }` 或
`{ "r": { "Err": { "error": "bad" } } }`。

Rust host 直接调用 `run_main` 时应传 `Value::variant_dict(...)`，或在业务层按自己的 JSON 约定先解码成 Relon `Value`。

### 用 `match` 分发

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

当分析器能静态推断被匹配值的 enum 类型时，会检查：

| 诊断 | 触发条件 |
| --- | --- |
| `NonExhaustiveMatch` | 缺少变体且没有 `*` 通配符 |
| `UnknownVariant` | 写了不存在的变体名 |
| `DuplicateMatchArm` | 同一个变体名出现了两次 |

无法静态推断时，相关检查交给运行期。想要不写穷尽分支，可以加 `*: ...` 通配符。

### 完整例子

```relon
#enum Order {
    Pending { customer: String },
    Shipped { tracking: String },
    Delivered { signed_by: String }
}

{
    summarize(Order o): o match {
        Pending:   f"待发货：${o.customer}",
        Shipped:   f"在途：${o.tracking}",
        Delivered: f"已签收：${o.signed_by}"
    }
}
```


## Schema 定义与契约守卫 (Identity Guards)

仅仅标记 `Dict` 有时是不够的，你往往需要验证字典内部长成了什么样子。这就是 `#schema` 指令登场的时候。

### 1. 定义 Schema

通过 `#schema` 定义的类型，其字段值将被视作**计算谓词 (Predicates)** 而非普通的数据。你可以使用 `*` 来代表「任意匹配」，或者使用一个闭包（函数）来进行自定义的业务校验。

`#schema` 有两种等价的写法：

```relon
{
    // 形式 A —— 独立声明（NameBody 形）：
    #schema ButtonConfig {
        // 类型必须是 String，内容任意匹配
        String type: *,

        // 自定义校验：宽度必须介于 10 到 100 之间
        #expect "Width must be between 10 and 100"
        Int width: (w) => w >= 10 && w <= 100,

        // 设置默认值
        #default false
        Bool disabled: *
    }
}
```

```relon
{
    // 形式 B —— 字段位置（dict-field 形）：
    // 适合「把 schema 写成同一个 dict 里普通字段」的代码风格。
    #schema ButtonConfig: {
        String type: *,
        #expect "Width must be between 10 and 100"
        Int width: (w) => w >= 10 && w <= 100,
        #default false
        Bool disabled: *
    }
}
```

两者完全等价；只在写法上区别——形式 A 不写冒号，作为一个独立的指令
出现；形式 B 走标准的字段：值语法。

### 2. 身份赋予 (Branding) 与名义类型

当一个普通的字典前面被标记了你定义的 Schema 类型时，魔法就发生了。

```relon
{
    // 为这个匿名对象赋予 ButtonConfig 身份
    ButtonConfig my_btn: { type: "submit", width: 50 }
}
```

- **校验**：引擎将立即对 `my_btn` 运行 `ButtonConfig` 定义的契约。
- **默认值注入**：因为我们在 Schema 中声明了 `#default false`，所以即使 `my_btn` 没有写 `disabled`，最终求值出的字典中也会被自动注入 `disabled: false`。
- **身份守卫 (Identity Guard)**：`my_btn` 被盖上了 `ButtonConfig` 的「品牌烙印」。以后无论是谁尝试通过深合并（`+` 算子或 `dict.merge`）修改 `my_btn`，修改后的结果都会被**再次全量强制校验**！

```relon
{
    // 在合并发生时，程序会立即报错："Width must be between 10 and 100"
    // 从而避免非法的属性污染你的业务结构
    invalid_btn: &sibling.my_btn + { width: 999 }
}
```

### 3. 指令位置的 Brand：`#brand X`

字段级类型标记 `Type field: { ... }` 只能写在「键的左侧」。但有些位置写不出键——列表元素、文档根、被其他指令（比如 spread 形 `#import`）包裹的 dict ——这时候就用 `#brand X`：

```relon
{
    #schema Weather {
        String location: *,
        Int temperature: *
    },

    // 等价于 `Weather typed: { ... }`，只是改写在指令位置
    decorated: #brand Weather {
        location: "Tokyo",
        temperature: 18
    },

    // 列表元素无法写字段级 hint，只能用 #brand
    cities: [
        #brand Weather { location: "Paris",  temperature: 20 },
        #brand Weather { location: "Sydney", temperature: 24 }
    ]
}
```

`#brand` 严格镜像字段级 hint 的运行时行为——同一个 `check_type` 校验、同一份 brand 写入逻辑，所以 `Weather typed: { ... }` 与 `decorated: #brand Weather { ... }` 在身份守卫、`match` 分发、JSON 输出上完全等价。

参数支持以下形态——基本上和字段级 type prefix 写得出的写法一一对应：

- **bareword**：`#brand Weather`、`#brand geo.Location`（路径用 `.` 分段）。
- **字符串字面量**：`#brand "Weather"`，与 bareword 解析为同一个类型名。
- **泛型形态**：`#brand Dict<String, Int>`、`#brand List<Weather>`、`#brand Foo<T>`。
- **可选修饰符**：`#brand Weather?`——和字段级 `Weather? w: ...` 行为一致，`None` 放行，其它走原类型校验。

> 关于泛型 brand 字符串：写入 `dict.brand` 的字符串和 `format_type_node` 输出一致。
> - 单段非内置类型：`Weather`。
> - 多段路径：`geo.Location`。
> - 泛型：`Dict<String, Int>`、`Foo<T>`。
> - 可选：`Weather?`。
>
> 在 `match` 分支里用 bareword 形式（`Weather: ...`）只能匹配单段非内置 brand；要在 brand 字符串完全相等的语义下匹配泛型，请用 `&self.brand == "Dict<String, Int>"` 这种字符串比较，或重新设计 schema 使其包一层命名类型（`#schema Counters Dict<String, Int>`）。

**校验侧的边界**：

- 应用到 dict：`check_type` 通过后写入 `dict.brand`；内置类型名（`Int`/`String`/...）的「**单段无泛型无 `?`**」形态只校验，不写 brand——和字段级 hint 完全一致。
- 内置容器的泛型形态（`Dict<K, V>`、`List<T>`）按照 `check_type` 既有规则递归校验，校验通过后 brand 字符串使用完整泛型表达式（如 `"Dict<String, Int>"`）。tagged sum 应使用 Rust-like `#enum`。
- 自定义类型 + 泛型参数（如 `Foo<T>`）：runtime 当前仅按 `Foo` 走 `check_custom_schema`，泛型参数在 brand 字符串里保留但**不参与运行时校验**。这一点和字段级 type prefix 完全一致。
- 应用到非 dict：仅校验，brand 无处可写。
- 同一处既写了字段级 hint 又写了 `#brand`（如 `Foo x: #brand Bar { ... }`）会被拒绝——同一个意图写两遍，去掉一个再说。
- `#brand Unknown` 在 `Unknown` 不在作用域时报 `VariableNotFound`，与 `Unknown x: { ... }` 一致。
- ⚠️ `#brand Map<...>` **不工作**：Relon 的内置容器命名是 `Dict`/`List`，没有 `Map`。`Map<...>` 会走 `check_custom_schema` 查找名为 `Map` 的 schema，找不到则报 `VariableNotFound`。

#### 在 schema 字段中使用

`#brand X` 也能写在 `#schema` body 内的字段上——此时它是字段级 type prefix `X` 的同义形式：

```relon
{
    // 这两个 schema 完全等价：
    #schema A {
        String name: *,
        Dict<String, Int> counters: *
    },

    #schema B {
        #brand String name: *,
        #brand Dict<String, Int> counters: *
    },

    // 实例校验走的是同一条路径
    A inst1: { name: "x", counters: { hits: 1 } },
    B inst2: { name: "y", counters: { misses: 2 } }
}
```

放在 schema 字段上时的额外规则：

- 字段同时写了 type prefix 和 `#brand` 会被分析器拒绝：`#schema S { #brand Bar Foo x: * }` 报 `SchemaFieldBrandConflict`。同一个意图不要写两遍。
- `#brand` 与 `#expect` / `#default` / `#msg` / `#error` 等元指令可以叠用：`#default 0 #brand Int age: *` 工作良好。
- 字段位置的 `#brand` 只影响 schema 字段的类型声明本身，**不会**给实例上的内嵌 dict 自动写 brand——和 `Type field: *` 形式一致。如果你希望实例上的内嵌 dict 也带 brand，请在实例那一侧再写一次 `#brand` 或 type prefix。

## Schema 混入与组合 (Mixins)

在组件库中，你常常需要通过基础配置扩展出高级配置。由于 Schema 也是头等值，你可以使用 `+` 直接将它们合并起来！

```relon
{
    #schema BaseControl {
        String id: *,
        #default false Bool disabled: *
    },

    // 继承 BaseControl 的约束，并混入额外的属性
    #schema IconControl &sibling.BaseControl + {
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
    #schema Menu {
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

## 自定义校验消息（#expect）

默认情况下，校验失败抛出的错误消息是引擎根据谓词字符串拼出来的，可读性一般。`#expect "..."` 让你显式指定一条业务可读的消息：

```relon
{
    #schema Server {
        #expect "Port must be between 0 and 65535"
        Int port: (p) => p > 0 && p < 65536
    },

    Server s: { port: 70000 }
    // → TypeMismatch { expected: "Port must be between 0 and 65535", ... }
}
```

`#expect` 一定跟一个谓词闭包搭配使用——给 `*` 加 `#expect` 是没意义的。

## 必填、可选、默认值、计算默认值

Relon 的 schema 字段把这四种语义放在同一个声明里，靠**修饰符的组合**区分：

```relon
{
    #schema User {
        // 1. 必填（默认行为）：缺失即抛错
        String name: *,

        // 2. 可选（? 后缀）：字段可以缺失；没有值请显式用 None
        String? bio: *,

        // 3. 字面量默认值（#default value）：缺失时填这个常量
        #default "user"
        String role: *,

        // 4. 计算默认值（#default (self) => ...）
        // 当字段缺失时调用闭包，self 是「已知字段已经填好的实例视图」
        #default (self) => self.name + " <unset>"
        String display_name: *
    },

    // 用法
    User u: { name: "Ada" }
    // u.bio 可以缺失；可选访问返回 None
    // u.role         == "user"
    // u.display_name == "Ada <unset>"
}
```

几条务必注意的细节：

- 显式写出的字段值**永远赢过**默认值——无论字面量还是计算式。
- 计算默认值是**惰性触发**的：只在字段确实缺失时才求值，不会白白调用。
- 计算默认值的 `self` 看得到「显式写出的字段 + 已经被字面量默认值填上的字段」，但**看不到其他计算默认字段**——它们之间不会互相观察，避免出现求值循环。

## 接下来

- 把 schema 和帮助函数封装成可复用的库：[模块与作用域](./modules.md)
- 在 Rust 端注册自己的 `#schema` 用得着的原生函数：[嵌入宿主](./host-integration.md)
- 跑不可信脚本时把 schema 与 `#expect` 当作第一道防线：[沙箱与权限](./sandbox.md)
