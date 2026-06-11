# 嵌入宿主

Relon 不是「装好就跑」的独立程序——它是一个 **Rust 可嵌入的 toolkit**。这一页讲怎么把它接进你自己的进程：解析、求值、注册原生函数、定制模块解析、控制 JSON 输出形态。

> 想要不可信脚本的安全策略？看完这页之后跳到
> [威胁模型](./threat-model.md) 和 [沙箱与权限](./sandbox.md)。

## 首发版嵌入路径

先选定一条路径：

| 路径 | 适用场景 | 后端 / 信任姿态 |
| --- | --- | --- |
| Sandboxed facade | 宿主把 Relon 当作“可计算配置”，所有数据都由宿主推入。 | `relon::from_str` / `EvaluatorBuilder` 默认值：sandboxed 姿态，无本地 import，无 staged host fn。 |
| Trusted host-owned script | 源码归宿主所有，并且需要本地 import 或 staged native fn。 | `Backend::TreeWalk` 加显式 trust/capability grant。这是首发版 staged host fn 注册的唯一推荐路径。 |
| Native performance path | 宿主希望兼容的 `#main(...)` 程序走编译执行。 | `Backend::Auto` 或显式 compiled backend，但不混用 staged host fn。首个公开版本会拒绝 `Backend::Auto + TrustLevel::Trusted`。 |

对于不可信插件、租户脚本或上传源码，把 Relon 放在 VM / 进程 /
容器边界之后运行。Relon 提供 capability 词汇和预算模型；真正硬边界由
Wasmtime、进程或容器限制执行。

## 推荐范式：Push-by-default

在动手集成之前，先确定一件**架构决策**：外部数据怎么进 Relon？

Relon 推荐的范式是 **push**——宿主在求值**之前**完成所有 I/O，把数
据净化成 `Value` 注入 `run_main(args)`；脚本通过
`#main(...)` 签名声明它**期望**的形状；整体保持纯函数
`(source, args) → output`：

```rust
// ✅ 推荐：push-style，#main 入口程序
use std::collections::HashMap;
use relon::{Backend, EvaluatorBuilder, TrustLevel, Value};

let user_data = http_client.get(&format!("/api/user/{user_id}")).await?;
let posts_data = db.query_user_posts(user_id).await?;

// 将 host-side 数据净化成 Value
let user_value: Value = serde_json::from_value(user_data)?;
let posts_value: Value = serde_json::from_value(posts_data)?;

let evaluator = EvaluatorBuilder::from_str(source)
    .backend(Backend::Auto)          // 默认值；自动在解释器与 AOT 间分派
    .trust(TrustLevel::Sandboxed)    // 默认值；显式写出信任姿态
    .build()?;

let mut args = HashMap::new();
args.insert("user".to_string(), user_value);
args.insert("posts".to_string(), posts_value);

let result = evaluator.run_main(args)?;
```

`serde_json::from_value::<Value>` 是无目标类型解码：JSON array 会解成
`Value::List`，JSON `null` 会被拒绝，因为 `null` 不是 Relon 值。直接写
Rust host 时，如果 `#main` 参数是 tuple，请构造 `Value::tuple(...)`；如果
参数是 enum，请构造 `Value::variant_dict(...)`，或按 `#main` 签名做目标
类型感知解码。只有已知目标类型是 `Option<T>` 时，JSON `null` 才会
映射成 `None`；`Option<T>` 的非 null 输入会解码成 `Some(value)`。

CLI 的 `relon run --args '<json>'` 和 WASM playground 的 `#main(args)` 已读取
入口签名：目标是 `Tuple<...>` 或 tuple schema 时，JSON array 会进入
`Value::Tuple`；目标是 `List<T>` 时仍进入 `Value::List` 并按 list 元素类型
校验；标量目标会拒绝不兼容的 JSON 形状；目标是 enum 时，JSON string 可以进入
同名 unit variant，带 payload 的 variant 使用外部标签对象。例如
`#enum Stat { Up, Down }` 的 `Stat` 参数可以用 `{ "s": "Up" }` 输入；
`#enum Msg { Email { address: String }, Pair(Int, String) }` 可以用
`{ "m": { "Email": { "address": "x@y.z" } } }` 或
`{ "m": { "Pair": [7, "x"] } }` 输入。`Option<Int>` 可用 `null`、`41` 或
`{ "x": { "Some": { "value": 41 } } }`；`Result<Int, String>` 可用
`{ "r": { "Ok": { "value": 41 } } }` 或 `{ "r": { "Err": { "error": "bad" } } }`。带 payload 的
variant 不从裸字符串解码。

脚本端配上一个 `#main(...)` 签名，描述 host 必须推进来的形状：

```relon
#main(User user, PostList posts)
{
    #schema User { String name: *, String tier: * },
    #schema Post { String title: * },
    #schema PostList List<Post>,
    summary: f"${user.name} has ${len(posts)} posts",
    eligible: len(posts) > 10 && user.tier == "gold"
}
```

`#main(Type name, ...) [-> ReturnType]` 是文件的**入口签名**，每个参数声明一个
host-pushed slot：

- 参数名是脚本里直接可见的根级绑定（注意：**不是** `input.user`，
  就是 `user`）；
- 参数类型必须是已声明的 `#schema` 或基础类型；
- runtime 在跑 body 之前会校验 `args` 与签名：缺字段 →
  `MissingMainArg`；多字段 → `UnexpectedMainArg`；类型不匹配 →
  `MainArgTypeMismatch`。

> **编译路径 — 结构化入参。** native 编译执行器（cranelift / LLVM）
> 与编译版 wasm 校验目标的 buffer 协议现已支持结构化 `#main`
> 入参，而不仅是标量。以下形态都与 tree-walk oracle 逐字节一致地流入：
>
> - 标量叶子（`Int` / `Float` / `Bool`）；
> - **`String`** 入参（如 host 读好、喂进来的文件内容）；
> - **tuple schema** 入参，例如 `#schema IPv4 (Int, Int, Int, Int)`：
>   宿主用 `Value::Tuple` 提供 payload，编译端按位置解码；
> - **`List<scalar>`**、**`List<String>`**、**`List<Schema>`**、嵌套
>   **`List<List<scalar>>`**，以及双层指针数组 **`List<List<String>>`** /
>   **`List<List<Schema>>`** 入参（经 `.length()` 或同级标量字段读出消费；
>   元素的内层记录 —— schema 子记录、内层 string/scalar list 记录，或内层
>   指针数组 list —— 会物化进 buffer tail 区，并递归重定位进父 buffer
>   坐标系）；
> - **用户 `#schema` 结构体入参**，其字段为标量、`String`、
>   `List<scalar>`、`List<String>`、`List<Schema>`、`List<List<scalar>>`，
>   或双层 `List<List<String>>` / `List<List<Schema>>` —— 即整包结构化
>   config 记录，含字符串、列表、record 列表与嵌套列表字段；
> - **嵌套 `#schema` 结构体字段** 的多段链式读取（`o.inner.x`，乃至
>   更深的 `c.b.a.v`）。两种字段声明写法都可用 —— 值位 `inner: Inner`
>   与前缀 `Inner inner: *` —— 每个中间段会先 rebase 到其子记录基址，
>   再读里层字段。
>
> **返回方向**上，编译后端会把 body 输出的 `Value` 重新 marshal 回
> buffer，与 tree-walk oracle 逐字节一致，覆盖：
>
> - 标量、`String`、`List<scalar>`（`List<Int/Float/Bool>`）顶层返回 ——
>   含直接 identity 返回标量 list 入参（其尾记录是单块 inline-fixed，
>   任意来源都能整块拷贝）；
> - **tuple schema 返回**（如 `#main() -> IPv4 = (127, 0, 0, 1)`）会解码为
>   `Value::Tuple`，输出投影为 JSON array；
> - **来自源码内 list 字面量**（`["a", "b", …]`）的 `List<String>` 返回 ——
>   这是 const-pool 块，内部 string 指针连续且单一基址，刚性尾拷贝能正确
>   重定位；
> - **从 `#main` 入参恒等返回 `List<List<scalar>>`**
>   （`#main(List<List<Int|Float|Bool>> xss) -> List<List<…>> = xss`），
>   **cranelift、llvm 与编译版 wasm 校验目标均已支持。** 这是「就地区走读
>   返回 ABI」承载的第一个形状：机器码不拷贝嵌套指针数组图，而是把结果根的
>   arena 偏移报给 host，host 在其来源区内 **校验后就地解码**（见下方设计
>   注记）；native 编译路径与 wasm 目标共用同一条 host 解码管线。wasm 上 host 直接读模块的
>   **线性内存**得到同一片 arena，并在解码前跑同一份 verifier（四方逐字节
>   相等：tree-walk == cranelift == llvm == wasm）。入参 **字段** 形式的
>   `List<List<scalar>>`（`#main(W w) -> List<List<Int>> = w.rows`）在这些
>   编译路径上 **同样支持**（F4）：arena-绝对槽约定下，字段读取直接把字段列表根
>   的 arena-绝对偏移压栈，故与恒等形走同一条就地返回；
> - **从 `#main` 入参恒等返回 `List<String>`**
>   （`#main(List<String> ss) -> List<String> = ss`），**cranelift、llvm
>   与编译版 wasm 校验目标均已支持。** 这是就地区走读返回 ABI 承载的第一个
>   **逐元素指针数组** 形状（也正是旧刚性尾拷贝会段错误的那种形状）：外层
>   `[len][off_i]` 头与各 `off_i` 指向的 `[len][utf8]` String 记录都在输入区
>   内，机器码报根偏移，host verifier 先逐个 String 记录在区内校验、再就地
>   解码 —— 逐字节等价于 tree-walk oracle（含每个字符串内容；CJK / emoji /
>   4 KiB 长串亦覆盖，wasm 上同样从线性内存经同一 verifier 读出）。入参
>   **字段** 形式的 `List<String>`（`#main(Outer o) -> List<String> = o.tags`）
>   在这些编译路径上 **同样支持**（F4，经同一 arena-绝对字段读取）；
> - **从 `#main` 入参恒等返回 `List<Schema>`**
>   （`#main(List<Cfg> items) -> List<Cfg> = items`），**cranelift、llvm
>   与编译版 wasm 校验目标均已支持。** 这是就地区走读最深的形状：外层
>   `[len][off_i]` 头指向每个 schema 子记录，子记录自身又带 `String` /
>   `List<scalar>` / `List<String>` 指针字段（及不同偏移的内联标量）。机器码
>   报根偏移，host verifier 递归走到 **每个子记录的字段指针层**（每个 off_i →
>   子记录头 → 每个 String / List 字段的尾记录）再就地把每个元素解成 branded
>   dict —— 逐字节等价于 tree-walk oracle（含每个子对象每个字段的内容），
>   wasm 上亦然（同一递归在线性内存上跑）。入参 **字段**
>   形式的 `List<Schema>`（`#main(W w) -> List<Cfg> = w.items`）在这些编译路径上
>   **同样支持**（F4，经同一 arena-绝对字段读取）；元素子记录自身再含
>   嵌套 `List<Schema>` / `List<List<…>>` 字段者（如 `Team { name: String,
>   members: List<Person>, tags: List<List<Int>> }`）**亦已支持，且递归到
>   任意深度**（F7）：就地子记录读取器经同一统一 list reader 递归，IR 转换
>   准入对元素 schema 的字段类型递归判定，故元素 schema 内的嵌套对象数组与
>   嵌套列表在任意嵌套深度上都逐字节等价解码；
> - **深层嵌套 schema 字段链返回**（`#main(Outer o) -> List<String> =
>   o.inner.tags`，乃至更深的 `o.a.b.tags`），**cranelift、llvm 与编译版
>   wasm 校验目标均已支持**（F6）。`≥3` 段链，中间各段为嵌套 schema 字段、
>   叶子为指针数组 list（`List<String>` / `List<Int|Float|Bool>` /
>   `List<Schema>` / `List<List<scalar>>`）：每个中间段读取对应子记录的
>   arena-绝对基址，再从该基址读叶子 list 根的 arena-绝对偏移 —— 与单段走读
>   共用同一单根哨兵 + 多区 verifier + reader，任意深度逐字节等价于 tree-walk
>   oracle。支持作顶层返回与对象字段（匿名 `Dict` / 结构体）；
> - **从 `#main` 入参返回 `List<List<String>>` / `List<List<Schema>>`**
>   （`#main(List<List<String>> xss) -> List<List<String>> = xss`，及
>   `List<List<Cfg>>` 形），**cranelift、llvm 与编译版 wasm 校验目标均已
>   支持**（F5）。这是**双层**指针数组形状：外层 `[len][off_i]` 头指向内层
>   指针数组 list 记录，每个内层记录自身又是 `[len][inner_off_j]` 头，其
>   entry 指向 `String` / schema 子记录。递归输入 marshaller 写出整张图，
>   重定位 walker 把内层指针数组再下钻一层 rebase，机器码报外层根偏移；host
>   verifier 递归走到**最内层每条记录**（外层 entry → 内层 list 头 → 内层
>   entry → String / schema 记录）再就地解码 —— 逐字节等价于 tree-walk
>   oracle（含每个内层元素的内容，CJK / 空 / 超长），wasm 上亦然。入参
>   **恒等**、入参 **字段** 走读（`#main(W w) -> List<List<String>> = w.rows`）
>   与作对象字段（匿名 `Dict` / 结构体）均支持；
> - **`#schema` 加品牌的结构体返回**（`#main() -> Cfg { ... }`），字段
>   可含上面列出的、已支持的结构体字段形态（含字面量 `String` / `List` 字段）；
> - **匿名 `-> Dict { ... }` 返回** —— 每个非 `#internal` 字段都会
>   marshal 进返回记录，含 `String`、`List<scalar>`、字面量 `List<String>`
>   字段；`#internal` 字段不进入 host 可见面（与 oracle 一致）。
>
> 编译后端仍 **暂不支持**（前置阶段以明确的 `unsupported type in
> #main` / `layout v1 does not yet support list element` 报错，绝不静默
> 回退；这类形状请改用 tree-walk 解释器）：
>
> - `Dict<_, _>` 入参（analyzer 无法给 `d["x"]` 下标定类型；结构化
>   config 请改用 `#schema` 结构体）。嵌套列表 / 对象数组 / 嵌套 schema
>   返回形状（恒等、入参字段、深层字段链、对象字段）现已在任意深度上四方
>   支持（F7）；tuple-return cap 在下面单列。
> - 超出 scalar/literal 包络的 tuple 返回：嵌套 tuple 元素、
>   `List<...>` / `Option<...>` / `Result<...>` tuple 元素，或 body 不是 tuple literal 的 tuple
>   return。这些在位置型 tuple-element 工作四方证明前仍是响亮 cap。
>
>   返回**含该类入参字段的对象** —— 无论对象是**匿名 `Dict`**
>   （`-> Dict { servers: servers, n: 1 }`）还是**结构体 `#schema`**
>   （`#schema Wrapper { servers: List<Server>, n: Int }`，经
>   `-> Wrapper { servers: servers, n: 7 }` 返回）—— 均**四方**支持
>   （tree-walk == cranelift == llvm == 编译 wasm 目标）。字段类型可为
>   `List<Schema>`、`List<List<scalar>>`、`List<String>` 或
>   `List<Int|Float|Bool>`（`List<Schema>` / `List<List<scalar>>` 在
>   cranelift 为 F1b、llvm 与 wasm 为 F2；F3 增加了结构体路径与标量/String
>   list 字段类型，四方齐通）。来源可为入参 **恒等**（`servers`），也可为
>   （F4）入参 **字段** 走读（`o.items`、`o.tags`）—— 二者都把字段 list 根的
>   arena-绝对偏移落进槽。对象头建在 `out_buf`，但入参来源字段的数据仍在
>   `in_buf` —— 这是真正的**跨区**字段指针。在 arena-绝对槽约定下，字段槽
>   **直接**存入参 list 根的 arena-绝对偏移（不拷贝 —— 注意这与源码内 list
>   **字面量**字段如 `tags: ["a", "b"]` 不同，后者拷进 `out_buf` 尾区、自洽于
>   该区）；解码前 host 先以 `out_ptr` 为锚对**整 arena** 跑**多区**对象
>   verifier，把槽指针判区到 input 区并对整张可达图界检（深至每个子记录的
>   String 字段），再由 `BufferReader::new_at_base` 跨区走读 —— 逐字节等价于
>   tree-walk oracle。wasm 上 host 从**线性内存**取同一片 arena 并跑同一份
>   verifier 门控的解码，无 wasm 专属路径。双层嵌套的
>   `List<List<Schema>>` / `List<List<String>>` 对象字段**同样支持**（F5）：
>   内层指针数组被再下钻一层重定位、verify 与读取。host 侧 **解码已就位**：
>   `BufferReader` 以单一基址走读 buffer，递归重建嵌套 `Value`
>   （`List<Schema>` 走 `read_list_record` / `read_list_record_at`，
>   `List<List<scalar>>` 走 `read_list_list` / `read_list_list_at`，就地
>   `List<String>` 走 `read_list_string_at`，双层指针数组走递归
>   `read_list_value` / `read_list_value_at`）。就地返回接线已覆盖 **逐元素
>   指针数组** 的 `List<String>` / `List<Schema>` 及双层
>   `List<List<String>>` / `List<List<Schema>>`，入参 **恒等** 与（F4/F5）
>   入参 **字段** 走读两形（见上）。
> - **返回来自 `#main` 入参函数调用 / 任意表达式（而非入参恒等、入参字段走读
>   或源码内字面量）的指针数组 list** —— 尚未逐字节证明可就地返回，故仍响亮
>   cap。
>
> 上述形状一律在编译期 **响亮报错**；编译后端绝不吐错数据、绝不崩溃 ——
> 请把它们路由到 tree-walk 解释器。

> [!NOTE]
> **为什么写出（output store）才是难点。** arena 是一整
> 块连续内存：`[const_data | in_buf @ in_ptr | out_buf @ out_ptr |
> scratch]`。运行中的机器码里每个指针都是 *arena 基址相对*（`arena_base +
> ptr` 即可解引用），所以 `in_buf` 里的入参图与 const-pool 字面量共享同一套
> 坐标。普通返回时 host 把 **out_buf 切片** 交给 `BufferReader`，于是返回
> 指针被当成 *out_buf 相对*。而一个入参恒等返回（`#main(List<P> xs) ->
> List<P> = xs`）整图都在 `in_buf` 内、内部偏移是 `in_buf` 相对的；旧返回
> 路径试图用单一刚性 delta 把该图 *拷贝* 进 `out_buf`，只有连续单基址的
> const-pool 块才成立，散落的入参图会段错误。
>
> **就地区走读返回 ABI（诚实的修法；`List<List<scalar>>`、`List<String>` 与
> `List<Schema>` 入参恒等形均已在 cranelift 与 llvm 两个后端上线）。** 不再拷贝，机器码通过 **负返回值哨兵** 把 **结果根的
> arena 绝对偏移** 报给 host：`run_main` 返回值 `>= 0` 仍是常规
> `bytes_written`（在 `out_ptr` 处解码），而返回值 `< 0` 编码
> `-(root_abs + 1)` —— 即「这是就地返回，根头在 arena 偏移 `root_abs`」。
> host 随后：
>
> 1. **选区**：用 `root_abs` 对 arena 布局边界（`const_data` / `in_buf` /
>    `out_buf` / `scratch`）比较定位所在区 —— 单区自洽不变式保证值整图自
>    含于恰好一个区，所以该区切片内部偏移即区相对；
> 2. **跑 verifier**（`verifier::verify_value_at`）走读整张可达图，边界
>    限定在该区内。任何越区指针、或跑出末尾的长度 / 偏移都 **响亮报错**，
>    绝不乱读。这是总开关：未过校验 host 绝不解码；
> 3. 仅在校验通过后 **就地解码**，复用与 out_buf 路径相同的
>    `BufferReader`（嵌套 list 根走 `read_list_list_at`，`List<String>` 根走
>    `read_list_string_at`，`List<Schema>` 根走 `read_list_record_at` —— 每个
>    子记录解成 branded dict），针对 verifier 刚认证过的那块区切片解码。
>
> 这样既保住承重的单区墙（不跨区拷贝、不整 buffer 刚性重定位），又把整类
> 「基址写错 / 散落图」bug 从 *静默错值* 变成 *明确的 verifier 失败*。host
> 解码管线（哨兵 → 选区 → verifier → 解码）只在
> `relon_eval_api::inplace_return` 落一份、由两个 AOT 后端共用，cranelift
> 与 llvm 走完全相同的总开关。读取器、verifier 与 **两个** native 后端的
> `List<List<scalar>>`、`List<String>` 与 `List<Schema>` 入参恒等形均已接通
> （verifier 递归走到每个 `List<Schema>` 子记录的字段指针层），对象字段返回
> 与编译版 wasm 校验目标的线性内存也走同一 ABI；剩余响亮 cap 是来自函数调用 / 任意表达式的
> 指针数组 list 返回，而不是入参恒等、入参字段或源码字面量。

### 入口边界 Result 与 Relon 值层 Result

宿主调用 `run_main` 拿到的是 Rust 一侧的
`Result<Value, RuntimeError>`：成功时 `Ok(value)`，失败时
`Err(...)`（schema 校验未过、运行时溢出、capability 拒绝等）。这条
**边界 Result 由 Rust 端承担**——脚本作者不感知。

`#main(...) -> ReturnType` 中的 `ReturnType` 描述的是 **body 产生
的 JSON 形态**（一个原子值、dict、list 或 tuple），不是 Result 包装。
Relon 内置的 `Result<T, E>` / `Option<T>` 是**值层**概念（建模数据
里某个字段「可能没有」/「可能失败」），不该出现在入口签名的返回
位置。

```relon
// 正确：ReturnType 描述 body 产生的 Json
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }

// 应避免：在入口边界写 Result —— 与 Rust 侧的 Result 重复记账
#main(Order order) -> Result<Order, String>
...
```

宿主代码侧：

```rust
match evaluator.run_main(args) {
    Ok(value) => /* value 是 ReturnType 描述的 Json */,
    Err(e)    => /* 校验/求值/能力错误 */,
}
```

这样写有几个一致好处：
- 「外部数据契约」写在 .relon 文件里，由 `#schema` 静态校验
- host 推数据缺字段 / 类型不匹配 → 求值开始前就报错
- 多个 schema 自然组合成入口签名（每个 slot 命名空间隔离）

对应的反面（**不推荐**作为默认）：

```rust
// ⚠️ pull-style：把 I/O 搬进求值过程
ctx.register_fn("http.get",
    NativeFnGate { network: true, ..Default::default() },
    Arc::new(HttpGet),
);
```

```relon
// 脚本内主动拉数据
{
    user: http.get("/api/user/" + user_id),
    posts: db.query("SELECT * FROM posts WHERE author = " + user.id)
}
```

### 为什么 push 优先

| 维度 | push | pull |
|---|---|---|
| 「同源 + 同输入 → 字节级一致」可兑现？ | ✅ args 是显式 `Value` 树，可重放 / diff / hash | ❌ args 隐式包含 `http.get` 当时的网络状态 |
| 测试 | 构造 args 即可 | 要 mock http / db client |
| 缓存 / 预编译 / fuzz | 真·纯函数，可 memoize | 任何缓存都跟时间和外部状态绑死 |
| 审计「这段逻辑会读到什么」 | 看一眼 `#main(...)` 签名 | 要 trace 所有 host fn reachability |
| 求值确定性（spec §1） | ✅ 只要 args 一致，结果一致 | ❌ 网络 / 外部状态随时间变化，结果无法重放 |
| 心智分工 | host 负责跨界 I/O，脚本负责数据组合，边界清晰 | 两者交织 |

### pull 不是禁，是「主动放弃求值确定性」

下面这些场景里 pull 仍合理：

- **延迟加载**：数据集大到全 push 不现实（「从 1M 用户里 filter」）
- **动态查询**：query 条件依赖脚本中间计算结果
- **副作用动作**：规则引擎判断后触发邮件 / 日志 / webhook —— 本来就要 side effect
- **观察性**：调试用 `@log("...")` 装饰器，不影响结果

这些场景下 host fn 用 [`register_fn`](#受-capability-门控的注册)
注册，按需声明 `NativeFnGate { reads_clock: true, network: true, ..Default::default() }`。
**这是有意识的取舍**：脚本作者主动放弃了「同一份 args 跑两次结果
必然一致」的承诺，换取了「能动态拉数据」。spec §1 的求值确定性只覆
盖 push 形态。

> **一句话总结**：能 push 就 push。只在 push 实在不可行时（数据量、
> 动态性、副作用）才用 pull，并且清楚知道这部分逻辑不再可重放。

## 入口程序 vs 库

是否声明 `#main(...)` 决定了文件**怎么用**：

| 声明 | 用法 | 入口求值 |
| --- | --- | --- |
| `#main(...)` | 入口程序 | `run_main(args)` 推参求值；`eval_root` **不做** `#main` 检查——会直接求值根表达式，形参未绑定，引用即报未定义名 |
| 无 `#main` | 纯数据库 / 共享 schema 库 | `eval_root(scope)` 直接求值；同时也可被 `#import`；对它调用 `run_main` 报 `NoMainSignature` |

库文件被 `#import` 时不需要 `#main`——`#import` 只取它的导出。这条
设计的好处：
- 库与入口的边界清晰，宿主不会把库文件当 entry 跑（拒之于门外）；
- 入口程序的 args 契约写在源码里，宿主无须额外约定。

## 最小例子

最常见的需求是「读一个 `.relon` 文件，拿一个 JSON 出来」。三行：

```rust
use relon;

let json = relon::json_from_file("config/app.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

如果 source 已经在内存里：

```rust
let json = relon::json_from_str(r#"{ host: "localhost", port: 8080 }"#)?;
```

> 顶层 `relon::*` API 走的是「无 `#main` 库 / 数据文件」的快路径
> （内部调 `eval_root`）。要跑带 `#main(...)` 的入口程序，请用
> `EvaluatorBuilder` 构建后调 `run_main(args)`。

想直接拿到一个反序列化好的强类型结构？走 serde：

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ServerConfig {
    host: String,
    port: u16,
}

let cfg: ServerConfig = relon::from_file("config/app.relon")?;
```

`relon::from_str` / `from_file` 内部就是 `json_from_*` + `serde_json::from_value`。

## 顶层 API 一览

| 函数 | 行为 |
| --- | --- |
| `value_from_str(src) -> Value` | 解析 + 求值，返回 Relon 内存值（含 closure / schema 等不能直接 JSON 化的形态） |
| `value_from_file(path) -> Value` | 同上，从文件读 |
| `json_from_str(src) -> serde_json::Value` | 求值 + 走默认 `JsonProjector` 投影到 JSON |
| `json_from_file(path) -> serde_json::Value` | 同上，从文件读 |
| `from_str::<T>(src) -> T` | 求值 + 投影 + serde 反序列化到自定义类型 |
| `from_file::<T>(path) -> T` | 同上，从文件读 |
| `analyze_from_str(src) -> AnalyzedTree` | **只**跑 parser + analyzer，不求值——用来给 LSP / CI 拿静态诊断 |
| `project_with(&projector, &value) -> P::Output` | 用自定义 `Projector` 处理已经求值的 `Value` |
| `project_from_str(src, &projector) -> P::Output` | parse + eval + 投影一气呵成 |

上表的每个求值入口都是**沙箱姿态**（filesystem `#import` 默认拒
绝、门控 native fn 不放行）。每个都有对应的 `*_trusted` 变体——
`from_str_trusted` / `from_file_trusted` / `json_from_str_trusted` /
`json_from_file_trusted` / `value_from_str_trusted` /
`value_from_file_trusted` / `project_from_str_trusted`——以受信任姿
态求值（等价于 `TrustLevel::Trusted`：filesystem `#import` 放行），
**只**用于宿主自有脚本。

### `EvaluatorBuilder`：选后端、选信任姿态、注册宿主函数

需要超出「一行拿 JSON」的控制（选执行后端、跑 `#main` 入口、注册
native fn）时，用 facade 的 `EvaluatorBuilder`：

```rust
use relon::{Backend, EvaluatorBuilder, ResourceBudget, TrustLevel};

let evaluator = EvaluatorBuilder::from_str(source)   // 或 from_file(path)
    .backend(Backend::Auto)        // Auto（默认）/ TreeWalk / CraneliftAot / LlvmAot
    .trust(TrustLevel::Sandboxed)  // Sandboxed（默认）/ Trusted
    .build()?;                     // -> Box<dyn relon::Evaluator>

let json_value = evaluator.eval_root(&Arc::new(relon::Scope::default()))?;
// 或入口程序：evaluator.run_main(args)?
```

- `Backend::Auto`（默认）会对平凡标量 `#main` 走 tree-walk 短路，
  其余形状惰性走 cranelift AOT，编译不支持的形状响亮回退 tree-walk
  ——详见 [性能](./performance.md)。
- 首个公开版本没有接通 `Backend::Auto + TrustLevel::Trusted`；builder
  会直接拒绝，而不是替宿主猜测。需要 trusted 本地 import 或 staged
  host fn 时用 `Backend::TreeWalk`；host-owned 源码若不需要 staged host
  fn，可选择显式编译后端。
- `register_native_fn(name, gate, fn)` / `register_pure_native_fn`
  在当前 builder surface 里只支持 tree-walk。需要 staged host fn 时
  用 `Backend::TreeWalk`；`Backend::Auto` / `CraneliftAot` / `LlvmAot`
  会响亮失败，而不是忽略这些函数。
- `max_source_bytes(n)` 在 parse 之前拒绝超过 `n` 字节的源码。这是
  parser/input guardrail，和 evaluator 的 step/value budget 分开。
- `resource_budget(ResourceBudget::dev())` /
  `ResourceBudget::untrusted()` 安装 evaluator 侧 step/value 预算。
  初版 API 要求 `Backend::TreeWalk`；其它后端会响亮失败，而不是静默
  忽略预算。强不可信执行应走 wasm runtime 和 engine 级限制。
  Wasmtime 的起点可以用
  `relon host-policy --target wasmtime --profile untrusted` 生成，见
  [威胁模型](./threat-model.md) 和
  [Wasmtime 宿主策略](./wasmtime-host-policy)。

```rust
let guarded = EvaluatorBuilder::from_str(source)
    .backend(Backend::TreeWalk)
    .trust(TrustLevel::Sandboxed)
    .max_source_bytes(256 * 1024)
    .resource_budget(ResourceBudget::untrusted())
    .build()?;
```

## `Context` 是什么

走 `relon::*` 顶层 API 或 `EvaluatorBuilder` 时，`Context` 在内部被构造好。如果你需要注册装饰器、自定义模块解析或逐项调 capability 旋钮，就要直接构造 `Context`，再交给具体后端类型 `TreeWalkEvaluator`（`Evaluator` 是各后端共用的 trait）：

```rust
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;
use std::sync::Arc;

let node = parse_document(source).unwrap();
let mut ctx = Context::sandboxed().with_root(node);

// （在这里注册函数 / 装饰器 / 替换 module resolver）

let value = TreeWalkEvaluator::new(Arc::new(ctx))
    .eval_root(&Arc::new(Scope::default()))?;
```

`Context` 持有：

- **`functions`** — 通过 `register_fn` 注册的原生函数表（纯函数走便捷封装 `register_pure_fn`）。
- **`decorators`** — 通过 `register_decorator` 注册的装饰器插件。
- **`module_resolvers`** — `#import` 走的解析器链；`Context::sandboxed()` 默认是 `[StdModuleResolver, FilesystemModuleResolver::default()]`。
- **`capabilities`** — 宿主授予的能力位（[沙箱与权限](./sandbox.md) 详解）。
- **资源预算** — 目前通过 `ResourceBudget` 桥接到 `Capabilities` 上的
  evaluator 兼容字段。
- **`root_node`** + **`analyzed`** — 根 AST 与 analyzer side-table（含 `#main` 签名）。
- **多份 cache**（path / module / loading）——避免重复求值。

> 历史说明：早期版本提供 `Context.input: Option<Value>` 和
> `with_input(value)` 作为 push 入口，已**移除**——push 现在统一走
> `run_main(args)`。再之前的 `Context.globals:
> HashMap<String, Value>` 通用注入点也已移除：多种语义混在一个 map
> 里会让破壳点散布；现在是单一入口 + `#main` 契约。

构造方式有两条主线：

| 构造器 | 默认安全等级 |
| --- | --- |
| `Context::sandboxed()` | 沙箱姿态：filesystem 默认拒绝、capability 全空、只剩 `std/...` 虚拟模块；单独使用并不是多租户边界 |
| `Context::new()` | 轻量基础构造器：只挂载虚拟 std 模块与内置纯函数；需要真实 workloads 时优先用 `Context::sandboxed()` 并显式授权 |
| `Capabilities::all_granted()` + `FilesystemModuleResolver::trusted()` | 宿主自有脚本的显式全开形态：filesystem 全开、门控 native fn 全放、无步数 / 大小预算 |

## 注册一个原生函数

最常见的需求：暴露一个由 Rust 算的常量或纯函数给 `.relon` 用。

```rust
use relon_evaluator::{Context, NativeArgs, RelonFunction, Value, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

struct AppVersion;

impl RelonFunction for AppVersion {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        Ok(Value::String(env!("CARGO_PKG_VERSION").into()))
    }
}

let mut ctx = Context::new();
ctx.register_pure_fn("app_version", Arc::new(AppVersion));
```

之后在 `.relon` 里：

```relon
{
    version: app_version()
}
```

要点：

- `register_pure_fn` 是 `register_fn(name, NativeFnGate::default(), fn)`
  的便捷封装：声明一个空 gate，任何 `Capabilities` 都能平凡满足，所
  以纯函数在沙箱下也能直接调。
- `NativeArgs` 同时拆好了 positional 和 named 参数：`args.get(0)` 拿位置参数，`args.get_named("name")` 拿命名参数。
- 函数返回 `Value`——Relon 的内存值类型；想构造 dict / list / tuple 用
  `Value::Dict` / `Value::List` / `Value::tuple(...)`。

## 受 capability 门控的注册

读文件、调网络、读环境这类**有副作用**的函数，用 `register_fn` 注
册时把对应的 `NativeFnGate` bit 标上：

```rust
use relon_evaluator::{Capabilities, Context, NativeFnGate, NativeArgs, RelonFunction, Value, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

struct ReadSecret;

impl RelonFunction for ReadSecret {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        let secret = std::fs::read_to_string("/etc/myapp/secret").unwrap_or_default();
        Ok(Value::String(secret.into()))
    }
}

// 沙箱下放行的方式：在构造期授予 gate 声明的每一个 bit
let mut caps = Capabilities::default();
caps.reads_fs = true;

let mut ctx = Context::sandboxed().with_capabilities(caps);
ctx.register_fn(
    "secret.read",
    NativeFnGate { reads_fs: true, ..Default::default() },
    Arc::new(ReadSecret),
);
```

每个原生函数都走同一条 gate 检查：函数声明的所有 bit 都必须在
`Capabilities` 里被授予，否则 `CapabilityDenied`。`register_pure_fn`
注册的纯函数声明的是空 gate，零 bit 缺失，所以不需要 capability 授
权也能跑；`register_fn(name, gate, fn)` 在 `gate` 含任何置位的 bit
时就需要宿主显式授予对应能力。`Capabilities::all_granted()` 一次把
六个 bit 全部打开。详见
[沙箱与权限](./sandbox.md)。

## 模块解析（Module Resolvers）

`#import <bindspec> from "path"` 不是直接读文件——它问 `Context` 的 resolver 链（`module_resolvers()` 可读取）上的每个 resolver 「你能解析这个路径吗？」第一个返回 `Some(ModuleSource)` 的赢，错误（`Err`）会立刻中断。

默认链：

1. **`StdModuleResolver`** — 解析 `std/list`、`std/string` 这些虚拟模块（嵌在 binary 里，零 IO）。
2. **`FilesystemModuleResolver`** — 从文件系统读：
   - host-owned 脚本可显式安装 `FilesystemModuleResolver::trusted()`，无 root 限制；
   - `Context::sandboxed()` 下使用 `FilesystemModuleResolver::default()`，**默认拒绝一切**——必须在它前面挂一个 `with_root_dir(...)` 实例才放行。

挂载示例（rooted resolver 排在默认拒绝的 resolver 之前，先到先得）：

```rust
use relon_evaluator::{Context, FilesystemModuleResolver};
use std::sync::Arc;

let mut ctx = Context::sandboxed();
ctx.prepend_module_resolver(Arc::new(
    FilesystemModuleResolver::with_root_dir("/var/relon-configs"),
));
```

`with_root_dir` 会把 root 路径 canonicalize，并在每次 import 时确认目标路径在 root 下面（包括防止符号链接逃逸）——细节见 [沙箱与权限](./sandbox.md#filesystemmoduleresolver-的行为)。

要插入自定义 resolver（比如「从内存读」「从 OCI registry 读」），实现 `ModuleResolver` trait 然后：

```rust
ctx.prepend_module_resolver(Arc::new(MyResolver)); // 走最前
// 想做 fallback：追加到链尾，仅在前面都不认时才被问到
ctx.append_module_resolver(Arc::new(FallbackResolver));
```

## 装饰器插件

**`@name(...)` 装饰器**只用于值变换，区别于结构 / 元数据用的
`#name ...` 指令（详见 [基础语法](./syntax.md)）：

- 内置：`@value(...)` 是唯一一个由 runtime 提供的装饰器名字；
- 用户定义：`@my_fn(arg)` 等价于把下方值传入 `my_fn` 的最后一个位置
  参数。`my_fn` 可以是同 dict 内的闭包、`#import` 进来的函数，乃至
  host 注册的 native fn——任何可调用的绑定都行；
- 宿主注册：实现 `DecoratorPlugin` trait 之后注册一个名字。

```rust
use relon_evaluator::{Context, DecoratorPlugin};
// 实现 trait 的细节略——3 个钩子全是 default no-op
ctx.register_decorator("my_org.audit", Arc::new(MyAuditPlugin));
```

`DecoratorPlugin` 提供三个钩子，全部默认 no-op，按需要 override：

| 钩子 | 触发时机 | 典型用途 |
| --- | --- | --- |
| `pre_eval` | 在被装饰节点求值**之前** | 注入 scope / 直接覆盖结果 |
| `wrap` | 在被装饰节点求值**之后** | 校验、转换（如 `@ensure.int`） |
| `schema_field_meta` | 从 schema 字典提取字段时 | 给字段挂元数据 |

trait 完整签名见 `crates/relon-evaluator/src/decorator.rs`，这里不抄一遍——大多数宿主只需要 `wrap`。

## `Projector`：定制 JSON 输出形态

默认的 `JsonProjector` 把 `Value` 投影成 `serde_json::Value`，处理细节：

- 闭包、schema、type、wildcard 在 dict 里**静默丢弃**（保留运行时元素，不污染 JSON）；
- 出现在顶层时**报错**（没法投影成 JSON）；
- 非有限浮点（`Infinity`/`NaN`）报错；
- `List` 与 `Tuple` 都投影成 JSON array；
- sum-type 变体输出**外部标签**形式：`{ "Email": { ... } }`；
- 普通 branded dict 保持**扁平**——`#schema User` 标过的 dict 不会被包一层。

想换一种输出形态——比如 sum-type 用 `{ "type": "Email", "address": "..." }` 内部标签风格，或者直接 BSON、Protobuf——实现 `Projector` trait：

```rust
use relon::Projector;
use relon_evaluator::Value;

struct InternallyTaggedJson;

#[derive(Debug, thiserror::Error)]
#[error("projection failed: {0}")]
struct ProjErr(String);

impl Projector for InternallyTaggedJson {
    type Output = serde_json::Value;
    type Error = ProjErr;

    fn project(&self, value: &Value) -> Result<Self::Output, Self::Error> {
        // 自己控制遍历，对 Value::Dict 看 brand/variant_of 改写形状……
        Err(ProjErr("指南省略自定义遍历实现".into()))
    }
}

let json = relon::project_from_str(source, &InternallyTaggedJson)?;
```

> **注意范围**：`Projector` 是「JSON 形状的微调旋钮」，不是「跳出 JSON 的逃生通道」。Relon 的输出永远要落到 JSON 上——这是它的硬约束。如果你想生成 YAML/TOML/XML，那是另一种工具的领域（比如 Pkl）。

## 构建期 AOT：`include_relon!` 与 relon-rs-*

除了运行时嵌入，还可以在 **构建期** 把 `.relon` 编译成可重定位目标
文件，链接进你的 Rust 二进制——`#main` 变成一个普通的 Rust 函数调
用，运行期不再有解析 / 求值开销。涉及三个 crate：

- **`relon-rs-build`** —— build.rs 侧的 `Compiler`，把每个 `.relon`
  源编成一个 ELF 目标文件（导出单个 extern 符号）+ 一个生成的
  binding `.rs`；
- **`relon-rs-macro`** —— `include_relon!` 过程宏，把对应 binding
  缝进你的源文件；
- **`relon-rs-shims`** —— 运行期 host shim（`SandboxState`、buffer
  协议入口、字符串算子等）。

```rust
// build.rs
fn main() {
    let out_dir = std::env::var_os("OUT_DIR").unwrap();
    relon_rs_build::Compiler::new()
        .source("src/foo.relon")
        .emit_all(&out_dir)
        .unwrap();
}
```

```rust
// src/main.rs
relon_rs_macro::include_relon!("src/foo.relon");
// 或起别名：relon_rs_macro::include_relon!("src/foo.relon" as compute);

fn main() {
    let state = relon_rs_shims::SandboxState::default();
    println!("{}", foo::main(&state, 42)); // #main(Int n) -> Int
}
```

当前接受的 `#main` 参数 / 返回叶子类型是 `Int` / `Float` / `Bool` /
`String` / `List<Int>`（权威清单是 `relon-rs-build` 的
`rust_type_for` 表）。端到端示例见 `crates/relon-rs-demo`。

## 错误类型

`relon::Error` 是 facade crate 的统一错误：

| 变体 | 来源 |
| --- | --- |
| `Error::Parse(String)` | 词法 / 语法错误 |
| `Error::Analyze(Vec<Diagnostic>)` | analyzer 错误**批量**返回（4 个 pass 一起跑完） |
| `Error::Eval(RuntimeError)` | 求值期错误：类型不匹配、未定义引用、capability 拒绝、step 超限等 |
| `Error::Io { path, source }` | 读文件失败 |
| `Error::Deserialize(serde_json::Error)` | `from_str::<T>` 类 API 反序列化失败 |
| `Error::NonFiniteFloat(f64)` | JSON 投影时遇到 `Infinity` / `NaN` |
| `Error::UnsupportedClosure` / `UnsupportedSchema` | 顶层就是一个 closure 或 schema，没法投影 |

`RuntimeError` 在沙箱模式下还会出现 `CapabilityDenied`、
`StepLimitExceeded`、`ValueTooLarge`——这些归属
[沙箱与权限](./sandbox.md)。入口程序还会出现 `NoMainSignature`（库
文件被当 entry 跑）、`MissingMainArg`/`UnexpectedMainArg`/
`MainArgTypeMismatch`（args 不匹配 `#main` 签名）。

## 接下来

- 不可信脚本的安全策略：[威胁模型](./threat-model.md) 和 [沙箱与权限](./sandbox.md)
- 让 `.relon` 端能用上你注册的函数：在 schema / library 里包装它们，参考 [类型与契约](./types.md)
- 错误的 miette 友好格式：直接把 `RuntimeError` / `Diagnostic` 喂给 `miette::Report`
