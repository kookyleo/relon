# 像 JSON 一样管理业务逻辑

业务逻辑可以像 JSON 一样存储、分发、审计，但仍然确定、可验证、可沙箱执行。

这句话是 Relon 的核心定位。它听起来像一个很小的工程愿望：不要把每一条
价格规则、工作流分支、风控策略、feature flag 都焊死在服务代码里。但往下
追一层，它其实是在回答一个更根本的问题：

> 当业务逻辑必须在多个系统、团队和版本之间流动时，它应该以什么形态存在？

普通 JSON 很擅长流动。它能进数据库，能走 RPC，能被 diff，能被 code review，
也能被任何语言解析。但普通 JSON 没有足够的表达力。真实业务配置不会永远停留
在常量表，迟早会长出条件、公式、默认值、校验、状态转移和上下文引用。

通用脚本语言很擅长表达。JavaScript、Lua、Python、Starlark 都能写规则。但脚本
一旦进入生产边界，就会把另一些问题带进来：它读不读环境变量？会不会碰网络？
能不能重放？不同机器上结果是否一致？用户提交的脚本是否可以安全执行？diff 里
看到一行变化，能不能判断它实际影响了哪些输入输出？

Relon 站在这两个世界之间。它承认业务逻辑需要表达力，但坚持让这种表达力保留
数据的形态。

## 为什么不是“再加几个 JSON 字段”

许多系统一开始都有一份很朴素的配置：

```json
{
  "discount": 0.1,
  "enabled": true
}
```

然后需求会自然生长：

- 金牌用户多减 3%。
- 满 500 减 5%，满 1000 减 10%。
- 欧盟用户展示 GDPR banner。
- 某个功能只给 25% 用户灰度。
- 工作流从 `paid` 到 `shipped` 时要发邮件和通知仓库。

这时你通常有三条路：

第一，把逻辑搬回服务代码。这样最稳，但每次改规则都要走代码发布，多个服务还
可能各自实现一份相似逻辑。

第二，继续把 JSON 扩成一套小型解释器，比如 `{ "op": "and", "args": [...] }`。
这样可存储、可传输，但可读性很快崩掉，业务作者写的是抽象语法树，而不是业务
规则。

第三，允许用户写脚本。表达力够了，但沙箱、确定性、审计和宿主边界都变成你要
自己兜底的问题。

Relon 的选择是第四条路：让业务逻辑保持 JSON-like 的数据外观，同时给它受控的
表达力。

```relon
#schema LineItem {
    String sku: *,
    #expect "qty must be > 0"
    Int qty: (n) => n > 0,
    #expect "unit_price must be >= 0"
    Float unit_price: (p) => p >= 0
}

#schema Order {
    List<LineItem> items: *,
    #expect "tier must be one of: standard / gold"
    String tier: (t) => t == "standard" || t == "gold"
}

#main(Order order)
{
    #private
    volume_rate(sub): sub >= 1000 ? 0.10: sub >= 500 ? 0.05: 0.0,
    #private
    loyalty_rate(tier): tier == "gold" ? 0.03: 0.0,

    subtotal: _list_reduce([
        it.qty * it.unit_price for it in order.items
    ], 0.0, (a, x) => a + x),
    discount_rate: volume_rate(&sibling.subtotal) + loyalty_rate(order.tier),
    total: &sibling.subtotal * (1.0 - &sibling.discount_rate)
}
```

这段逻辑可以存在配置中心，可以随 RPC 传输，可以被审计，也可以由宿主进程在沙箱
里求值。它不是一段“偷偷混进配置里的代码”，而是一份带契约的数据。

## 可存储：逻辑不再绑定二进制

把业务逻辑编进服务二进制里，最大的问题不是代码不好写，而是发布单位太粗。

一个折扣规则改动，本质上可能只是配置发布；但如果它嵌在服务代码里，就会被迫
进入完整工程发布流程：改代码、跑 CI、部署、灰度、回滚预案。对于高频变动的
业务规则，这个流程太重。

Relon 把规则变成可以存储的文档。你可以把 `.relon` 放在数据库、配置中心、对象
存储或仓库里。宿主服务只需要嵌入运行时，在求值时加载对应版本。

这带来一个直接后果：规则发布和服务发布可以解耦。

服务负责稳定能力：数据获取、权限、原生函数、外部副作用。Relon 文档负责变化
频繁的业务决策：价格怎么算、流程怎么走、策略怎么判、UI 描述怎么生成。

## 可分发：同一份规则穿过多个边界

业务逻辑一旦成为数据，就可以自然跨边界流动。

平台团队可以提供 `.relon` 库，里面放共享 schema、通用函数和装饰器约定。业务
团队写入口文件，通过 `#import` 复用这些稳定词表。一个服务里用的规则，也可以
原样发给另一个服务、一个边缘节点或一个离线审计任务。

关键是，规则在传输中不丢契约。

Relon 的 `#schema` 不只是文档注释。它会在运行时检查字段形状、类型、业务谓词、
默认值和品牌身份。一个 `Order`、`Transition`、`User` 被传到哪里，它的约束就
跟到哪里。

这和“传一段脚本字符串过去”不同。Relon 文档同时携带：

- 规则本身。
- 输入边界，通过 `#main(...)` 描述。
- 数据契约，通过 `#schema` 描述。
- 输出形态，最终投影成 JSON。

因此它更接近“可执行契约”，而不是“远程执行一段任意代码”。

## 可审计：diff 应该解释业务变化

业务规则需要被审计，不只是因为安全，也因为组织协作。

价格变化要能追踪。风控规则要能复盘。feature flag 要能知道谁开了什么、为什么
开、什么时候开。工作流状态机要能看出新增了一条边，还是改掉了一条已有边。

如果规则散落在服务代码、数据库字段、脚本片段和隐式环境变量里，审计就会变成
考古。Relon 的目标是让业务变化尽量集中在一份数据形态的文档里。

看一份工作流配置，审计者应该能直接看到状态表：

```relon
#schema Transition {
    String from: *,
    String on: *,
    String to: *,
    List<String> emit: *
}

#main(String state, String event)
{
    #private
    transitions: [
        #brand Transition {
            from: "placed",
            on: "pay",
            to: "paid",
            emit: ["charge_card", "log_payment"]
        },
        #brand Transition {
            from: "paid",
            on: "ship",
            to: "shipped",
            emit: ["notify_shipper", "email_user"]
        }
    ],

    #private
    matched: _list_filter(
        &sibling.transitions,
        (t) => t.from == state && t.on == event
    ),

    next_state: len(matched) > 0 ? matched[0].to: state,
    emit: len(matched) > 0 ? matched[0].emit: ["unhandled_event"]
}
```

这里的审计对象不是“某段代码可能调用了什么”，而是“状态表发生了什么变化”。
Relon 不执行邮件发送、不扣款、不写数据库。它只返回决策和待执行动作，真正的
副作用由宿主解释。

这条边界很重要：Relon 让决策可审计，把行动留给宿主系统。

## 确定：同源 + 同输入 = 同输出

如果业务逻辑要像数据一样被缓存、重放、hash、审计和分发，它必须确定。

Relon 的设计约束是：

> 同一份 source，加同一份输入，得到字节级一致的输出。

这不是宣传语，而是语言设计里的硬约束。默认情况下，脚本不能读取环境变量，
不能读取系统时间，不能访问网络，不能使用随机数，也不能依赖哈希表迭代顺序。
字典顺序、浮点行为和标准库行为都要让同一次求值可以复现。

确定性带来的工程价值很具体：

- 可以把一次线上决策拿到离线环境重放。
- 可以对规则和输入做 hash，把输出缓存起来。
- 可以在 CI 或审计任务里验证规则升级前后的差异。
- 可以让边缘节点和中心服务对同一份规则给出一致结果。

如果某个场景确实需要读网络、读时钟或调用宿主函数，Relon 也允许，但这必须通过
capability 显式打开。也就是说，放弃确定性是一项可见的架构选择，而不是脚本
悄悄做了什么。

## 可验证：契约和逻辑在一起

静态 JSON 最大的弱点，是字段之间没有内建关系。

`qty` 必须大于 0，`tier` 必须在固定集合内，`Transition.to` 必须是合法状态，
`Notification.via` 必须是 Email/SMS/Push 之一。这些约束如果只写在外部文档或
宿主代码里，就很容易漂移。

Relon 把契约写在规则旁边：

```relon
#schema User {
    String id: *,
    #expect "region must be one of: us / eu / apac"
    String region: (r) => r == "us" || r == "eu" || r == "apac",
    #expect "plan must be one of: free / pro / enterprise"
    String plan: (p) => p == "free" || p == "pro" || p == "enterprise"
}
```

这让错误尽早出现。宿主推进来的 `args` 不符合 `#main(User user)`，求值开始前就
会失败。某个列表元素被 `#brand Transition` 标记，加载时就会检查四个字段是否
满足契约。schema 不是另一个系统里的旁路配置，而是 Relon 文档的一部分。

这种验证能力服务的是两个角色：

- 平台团队用 schema 和宿主扩展定义稳定业务词表。
- 业务团队用这些词表拼装规则，错误在边界被拦截。

这就是 Relon 的双层作者模型：平台提供语言里的名词和动词，业务组合它们。

## 可沙箱执行：能力必须显式授权

一门可嵌入语言如果要运行用户或租户提交的逻辑，默认安全边界必须清楚。

Relon 的默认姿态是零环境特权。脚本不能自己提升权限，也没有一个隐式的 trusted
fallback。文件系统、网络、时钟、环境变量、随机数和宿主原生函数，都要由宿主
通过 `Capabilities` 显式授予。

这让 code review 看到的是明确的授权：

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.max_steps = Some(100_000);
ctx.capabilities.max_value_elements = Some(10_000);
ctx.capabilities.reads_fs = true;
```

宿主可以选择全关，适合租户规则、在线 playground、边缘策略。也可以为自有脚本
显式全开，适合构建期或内部工具。关键是，授权发生在宿主代码里，而不是脚本
自己决定。

这也是 Relon 和通用脚本语言的核心区别之一：Relon 不追求成为一门可以做任何事
的语言。它追求的是在可审计的边界内表达业务决策。

## 设计动机：承认配置会长出逻辑

很多配置系统的问题，不是它们太弱，而是它们假装自己永远不会变复杂。

业务系统里，配置会自然长出逻辑。折扣规则会依赖用户等级和金额。风控策略会依赖
地区、设备、历史行为。feature flag 会依赖租户、百分比、白名单。工作流会依赖
当前状态和事件。UI 配置会依赖上下文和相邻字段。

Relon 的设计动机不是阻止这些复杂度，而是给它一个合适的容器：

- 像 JSON 一样存储和传输。
- 像表达式语言一样计算。
- 像 schema 系统一样验证。
- 像沙箱 VM 一样受控执行。
- 像纯函数一样可重放。

这就是“Logic as Data”的完整含义。不是把代码伪装成配置，也不是把配置压扁成
常量表，而是让业务逻辑以数据的形态存在，并在工程边界内保持可控。

## 适合 Relon 的问题

Relon 尤其适合这些场景：

- 规则变化频繁，但宿主服务能力相对稳定。
- 同一份业务逻辑要在多个服务、任务或边缘节点之间共享。
- 需要让非核心工程团队编写或审核规则。
- 必须能重放一次历史决策。
- 需要在不可信或半可信环境里运行自定义规则。
- 希望规则 diff 能直接表达业务变化。

典型例子包括定价公式、校验规则、订单状态机、feature flag、风控策略、UI 描述、
边缘控制策略和自计算资产表。

不适合的场景也同样明确。如果你只需要静态配置，普通 JSON 足够。如果你需要大量
I/O、循环副作用和通用脚本能力，Lua、JavaScript 或 Starlark 可能更直接。如果你
只需要纯约束求解，CUE 这样的工具更贴近问题。

Relon 的边界不是缺点，而是产品定义：它把业务决策放进数据里，把外部世界留给
宿主。

## 最后一层：为什么是 JSON

JSON 是今天软件系统里最通用的边界格式之一。它不优雅，但它稳定、朴素、到处
可用。Relon 借用的是 JSON 的流动性，而不是停留在 JSON 的表达力。

所以那句话可以再写得更尖锐一点：

> Relon is logic of the JSON, by the JSON, for the JSON.

属于 JSON，因为它最终服务的是数据边界。  
由 JSON 表达，因为它保持数据文档的可读、可传、可审计。  
为 JSON 求值，因为宿主要的最终结果仍然是一棵清晰的 JSON value。

这不是为了发明另一种写配置的语法，而是为了让已经不可避免的业务逻辑，有一个
能被存储、分发、审计、验证、重放和沙箱执行的形态。
