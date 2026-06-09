# WASM Binary Layout v1（2026-05-16）

> Phase 0 子项 2/8。锁定 host ↔ wasm 边界的二进制 layout 协议。
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> 决策 1 "Binary memory handshake"。
>
> 本文档 v1 = 第一个发布 ABI 版本。后续修改必须 bump `abi_version`
> 字段，host SDK 拒绝加载 mismatch 模块。

## 协议概览

```
host                                         wasm module
─────────────────────────────────────────────────────────
build BufferBuilder                          codegen sees #main schema
  layout::offsets_for(#main schema)            -> emits offset table
  → in_buf bytes  ────────────────────────→  i64.load offset=N etc.
                                            run_main(in_ptr, in_len, out_ptr)
out_buf bytes ←────────────────────────       writes result by offset
                                              returns bytes_written
parse out_buf via layout::reader(...)
```

Host 写、wasm 读；wasm 写、host 读。**全过程零 serde**。

## ABI Version

`wasm-srcmap-section-v1` 同位的另一个 custom section：

```
custom "relon.abi":
  abi_version: u16 = 1
  codegen_version: u32 = <semver-numeric>
  schema_hash: [u8; 32]  ;; sha256 of #main schema in canonical form
```

Host SDK 在 instantiate 时检：

1. `abi_version` 必须 == host SDK 的预期值；不等就 refuse-to-load
2. `schema_hash` 必须 == host SDK 看到的 #main schema hash；不等说明
   schema drift，refuse-to-load（避免 host 用旧 schema 写新 wasm 期望
   的 layout）

## 基础类型 layout

每个 leaf-type slot 固定大小、固定对齐。下表是 **slot size**——即
该字段在父结构里占的连续字节数（含 padding）。

| Relon Type | wasm 表示 | size (bytes) | align (bytes) |
| --- | --- | ---: | ---: |
| `Null` | tag-only | 1（pad 到 slot 边界） | 1 |
| `Bool` | u8 = 0/1 | 1（pad 到 slot 边界） | 1 |
| `Int` (i64) | s64 LE | 8 | 8 |
| `Float` (f64) | f64 LE | 8 | 8 |
| `String` | inline header + bytes | variable，见 § String layout | 4 |
| `List<T>` | inline header + element area | variable，见 § List layout | 4 |
| `Dict / branded Schema` | flat record | sum of field slots（带 padding） | 8 |
| `Option<T> / Result<T, E>` | tag + payload | 1 (tag) + sizeof(T) max(sizeof(E)) | 8 |

### slot 对齐通用规则

任何 nested 结构的字段按声明顺序排列；每个字段起始 offset 调到 **该
字段类型 align 的整数倍**（向上对齐），中间空字节填 0x00。

例：`#schema User { String name: *, Int age: * }` →

```
offset  field        type     size  notes
─────────────────────────────────────────────────────────────────
  0     name         String   var   inline (see § String layout)
  ?     [padding]    -        ?     align up to 8
  ?     age          Int      8     i64.load offset=<aligned>
```

`name` 是 String，长度可变。但是 wasm 静态生成的 `i64.load offset=N`
指令需要 **N 是编译期常量**。所以 layout 必须确保每个字段 offset
**只依赖于该字段之前其他字段的 layout**。

→ String / List / Dict 这种 variable-size 字段在 record 里**不能 inline**；
要么放到末尾（最多一个 variable-size 字段在末尾，offset 仍是常量），
要么用 **pointer-indirected** 表示。

## Pointer-indirected variable fields（关键技巧）

固定 record 里**每个 variable-size 字段表示成 (u32 offset)**——其中
offset 是相对于 buffer 起点的字节偏移。实际字节数据存在 record
之后的 "tail area"。

```
record area    (size 编译期定)         tail area    (variable)
[ name_offset ][ age      ][...]       [ name_len ][ name_bytes ]
   u32           i64                       u32        n bytes
   ^^^^^^^^
   wasm reads: i32.load offset=0
   then jumps to that offset
```

固定 record 字段的 offset 完全编译期可解析。wasm 读 String 的指令是：

```wasm
;; load the offset stored at field 0
(local.get $buf_base)
(i32.load offset=0)
;; that's the byte offset of the String header in the tail area
;; from there:
;;   header[0..4] = u32 len
;;   header[4..] = utf-8 bytes (no trailing nul)
```

### record 里 nested record

同样是 pointer-indirected：

```
record:
  [ user_ptr ][ cart_ptr ]
     u32         u32

tail:
  user_ptr -> [ name_ptr ][ age ]
                u32         i64
              name_ptr -> [ name_len ][ name_bytes ]
                            u32          n bytes
  cart_ptr -> [ id ][ qty ]
                i64    i64
```

注意 `Cart` 里没有 variable-size 字段（id / qty 都是 Int），但 cart
**整体** 仍然作为 pointer 进入 root record——因为：

1. host 端预分配 buffer 时不知道 cart 是否最终带 variable 字段（future
   schema evolution）。永远 pointer-indirect 让 layout 演化兼容性最好
2. wasm 端 codegen 简单——任何 nested 结构都是一次 `i32.load` 拿
   pointer 再 `iN.load offset=...` 拿字段

**例外**：`Int` / `Float` / `Bool` / `Null` 这种**固定 size 的叶子**
在父 record 里**永远 inline**，不进 pointer。否则每个 Int 多 4 字节
overhead 太亏。

## String layout

```
[len: u32 LE][bytes: u8 * len]
```

- `len` 是 **字节数**，不是 Unicode codepoints（utf-8 string，沿用 Rust /
  Relon 当前 `String` 的字节语义）
- **无 trailing nul**——Relon String 是 bytes-with-length，不是
  C string
- **最大长度** = `2^32 - 1` 字节（4 GB）；超过 trap with
  `StringTooLong { range }`
- 多个 String 在 tail area 紧排（无 padding），但 String header 自身的
  `u32 len` 字段 4 对齐

## List layout

```
[len: u32 LE][element_0][element_1]...[element_(len-1)]
```

固定 size 元素（`List<Int>`、`List<Float>`、`List<Bool>` 等）**inline**
存储：

```
List<Int> with [10, 20, 30]:
  [3: u32][10: i64][20: i64][30: i64]
  ^      ^
  len    elements area (i64 stride)
```

可变 size 元素（`List<String>` / `List<User>` / `List<List<T>>`）走
**pointer 数组**：

```
List<String> with ["abc", "de"]:
  [2: u32][offset_0: u32][offset_1: u32]
  tail:
    offset_0 -> [3: u32][a,b,c]
    offset_1 -> [2: u32][d,e]
```

`len` 4 对齐；后面 element area 起始位置按元素 align 向上调整。

**最大 List 长度** = `2^32 - 1` 元素；超过 trap with `ListTooLong`。

## Dict / branded Schema layout

已 brand 过的 dict（即 typed schema）走**字段顺序展平 record**，
按 schema 声明顺序排，使用 § "对齐通用规则"。

**未 brand 的 dict**（`Value::Dict { brand: None, ... }`）——MVP 不
支持作为 `#main` 入参 / 返回类型。`analyzer` 已经 ban；wasm codegen
不需要处理。

## Option<T> / Result<T, E> layout

```
Option<T>:
  [tag: u8][padding to T align][T payload]

  tag = 0  ⟶ None  (payload bytes meaningless)
  tag = 1  ⟶ Some(T)

Result<T, E>:
  [tag: u8][padding to max(T,E) align][T or E payload]

  tag = 0  ⟶ Ok(T)
  tag = 1  ⟶ Err(E)
```

Payload 是 inline 的，按 `max(T align, E align)` 对齐。**不**走
pointer-indirect——Option / Result 是 tag-major union，host 端写
入时已知 tag，直接写完整 payload。

Variant 大小不等时（`Result<Int, String>`），padding 填到最大 variant
的大小（结构整体 size = `1 + max(sizeof(T), sizeof(E))`）。

## Enum / Sum types layout

Rust-like `#enum Notification { Email { ... }, SMS { ... } }` 编译成 tag +
inline largest variant：

```
[tag: u8][padding][largest_variant payload]

tag = 0  ⟶ Email payload
tag = 1  ⟶ SMS payload
```

变体多于 256 个时 tag 升 u16（v1 不预期这种情况，超过 256 codegen
报错并要求 schema 拆分）。

## Out-buf 写入约定

wasm 写返回值用同样的 layout。Host 必须**预分配足够大的 out_buf**。

Host 端两阶段策略：

1. **dry-run**：调 `run_main(in_ptr, in_len, 0)`，wasm 在 trap 之前
   先把"所需字节数"算出并返回（如果 out_ptr=0 就只算长度不写）。
   实际上 v1 走更简单的方式（见下）
2. **fixed-size root**：v1 要求 `#main` 返回类型的**根 size** 必须是
   静态可算的（即 root 不是 String / List 这种 variable）。host SDK
   按 schema 预分配 root size 的 buffer

如果 return type 是 String / List（在 v1 也允许）：

- root 4 byte = `total_bytes_used` (u32) — wasm 先写这个值
- 之后是实际数据，host 看 root 4 byte 知道实际多大

host SDK 默认 grow strategy: 先尝试 4 KB buf，不够（wasm trap
`OutBufTooSmall`）再 grow 翻倍重试。trap kind 通过 custom section
里的 `error_classification` 表 host 端查得到。

## Endianness

**所有多字节整数 little-endian**（与 wasm spec 一致；x86_64 / ARM64
原生即 LE）。

## Padding 字节值

`0x00`。不依赖。Host SDK 不应该 assume 任何 padding 内容。

## Trap kinds（与 wasm-srcmap 配合）

wasm 出错时 trap with 特定 kind。host 端通过 wasm runtime trap 类型 +
custom section srcmap 翻译为 `RuntimeError`：

- `IntegerDivByZero` → `RuntimeError::DivisionByZero { range }`
- `OutBufTooSmall` → `RuntimeError::OutBufTooSmall { needed_bytes }`
- `CapabilityDenied(bit)` → `RuntimeError::CapabilityDenied { caps }`
- `StringTooLong` / `ListTooLong` → `RuntimeError::ValueTooLarge`
- `StepLimitExceeded` → `RuntimeError::StepLimitExceeded`
- `BadSchemaHash` / `BadAbiVersion` → instantiate 拒绝（不是 runtime
  trap）

完整 trap → RuntimeError 表见 Phase 7 输出的
`wasm-trap-mapping` 文档（未写）。

## v1 不包括（明确排除，留 v2+）

- **动态 Schema**：在运行时改 schema 不支持。v1 假设 schema 编译期
  确定
- **GC heap**：所有 buffer 是 host-allocated；wasm 不 GC。变长结构
  用 pointer 索引，不用引用计数
- **Closure 跨边界**：见 [ADR-A](./wasm-adr-A-closure-boundary-2026-05-16.md)
- **NaN / Inf 特殊处理**：`Float` 按 IEEE 754 直接传；NaN 行为同
  tree-walker
- **schema 演化**：v1 schema_hash mismatch 直接 reject。v2 才会考虑
  field-add / field-remove 的兼容规则

## Phase 0 checklist 推导

实施 binary handshake 需要：

- [ ] `relon-eval-api::layout` 模块——`SchemaLayout::offsets(&Schema) ->
  OffsetTable`、`SchemaLayout::root_size(&Schema) -> usize`
- [ ] `relon-eval-api::buffer` 模块——`BufferBuilder` typesafe writer +
  `BufferReader` typesafe reader（host SDK 用）
- [ ] `relon-ir` 的 IR ops 携带 `(offset, size, align)` triple 给
  codegen 用
- [ ] `relon-codegen-wasm` emit `i64.load offset=N` / `i32.store offset=N`
  等指令时直接读 `OffsetTable`
- [ ] custom section `"relon.abi"` 在 `wasm-srcmap-section-v1` ADR
  里规定格式（见下一份）
