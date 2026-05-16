# ADR-D：Schema 验证在 host 侧还是 wasm 侧（2026-05-16）

> Phase 0 子项 7/8。
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> §二 待定子问题 D "Schema 验证在哪一侧做"。
> 依赖：[`wasm-binary-layout-v1`](./wasm-binary-layout-v1-2026-05-16.md)。

## Context

决策 1 的 binary memory handshake：

- host 按 schema 算出的 offset table 把 `#main` 入参字节写进 `in_buf`
- wasm 直接 `i64.load offset=N` 读

**问题**：如果 host 写的字节不符合 schema（例如 String len 字段比实际
bytes 多，Bool 字段是 0xFF 而不是 0/1，Int 字段越界）：

- wasm 端不验证 → 读出垃圾数据 → undefined behavior
- wasm 端验证 → 每次 boundary crossing 都付 microseconds 级开销

谁负责 schema 完整性验证？

## Decision

**走 D2：host SDK 强制类型安全 + wasm 假定 host 写对**。具体：

1. host 端不允许"裸字节写 in_buf"——必须走 `BufferBuilder` typesafe
   API，Rust 类型系统保证每个字段类型 / range 正确
2. host 端的 schema validation（content invariants，比如 `where age > 0`
   这种 predicate）在 `BufferBuilder::build()` 调用前 / 内做
3. wasm 端 **不** 重复 layout-level 验证；运行期任何 layout 不一致都
   是 host SDK bug 导致的 UB（host 责任）
4. **运行期 predicate 验证**（如 `where amount > 0`）走 schema-rooted
   method 路径——与 tree-walker 一致——wasm 端 codegen emit predicate
   call，验证失败 trap，translate 为 `RuntimeError::SchemaPredicateFailed`

## Rationale

### 1. binary handshake 的整个性能价值就在"零拷贝零验证"

如果 wasm 端再做一遍 layout-level scan / utf-8 well-formedness check /
list bounds check，binary handshake 跟 JSON 没区别——验证占了大部分
开销。**信任 host 写对**是这个 ABI 模型的基本前提。

### 2. host SDK typesafe 已经能 enforce 99% 的 layout correctness

```rust
// host SDK API (Phase 0 输出之一)

let mut buf = BufferBuilder::new(&schema);
buf.write::<String>("name", "Alice")?;     // 类型不匹配编译期失败
buf.write::<i64>("age", 30)?;
buf.write_nested("addr", |nested| {
    nested.write::<String>("street", "123 Main St")?;
    nested.write::<String>("city", "Springfield")?;
})?;
let in_buf = buf.finish();
```

Rust 类型系统 + `BufferBuilder` API 设计：

- 类型匹配编译期检查（`write::<i64>` 不能写到 String 字段）
- 字段名编译期 enforce（v1 走 string-keyed，v2 可以 macro 出 typesafe
  field accessor）
- String 长度 / UTF-8 wellformedness：在 `BufferBuilder` 里 enforce，
  写之前已经 valid

→ 来自 host SDK 的字节**结构性正确**就是 byte-exact 的 layout
匹配；wasm 端不需要重复检查。

### 3. content invariant validation 与 tree-walker 一致

tree-walker 当前 schema validation 分两层：

- **layout 层**：dict 字段名 / 字段数量 / 类型 tag 校验——这层由
  host SDK 替代（决策 1 binary handshake 不存在 dict-with-extra-fields
  这种问题，layout 是固定的）
- **predicate 层**：`where age > 0`、`#expect` 等运行期断言——这层
  wasm 端 codegen emit 调用，与 tree-walker 行为一致

wasm 端不做 layout validation 不是"放弃验证"——而是"layout 验证在
host SDK，predicate 验证还在"。**用户感知的 schema validation 严格
程度跟 tree-walker 完全一致**。

### 4. 真 UB 风险有限

担心"host SDK 有 bug 写错字节，wasm 读出垃圾，runtime panic"：

- wasm linear memory 是 sandboxed 的——任何越界 load / store **wasm
  runtime 自身就会 trap**，不会出 host 进程
- 即便垃圾数据落在 wasm 自己的 memory 里，最坏后果是输出值错误 →
  predicate 验证捕获 → 报错
- 不存在"wasm 读垃圾数据触发 host 进程 segfault"——这是 wasm
  sandbox 模型的根本属性

→ host SDK bug 的爆炸半径**限制在该次 eval 调用**，不影响其他
请求 / 其他模块。这是可接受的风险水平。

### 5. wasm 端如果真要验证，开销不可忽略

测算：

- 一次 utf-8 well-formedness 扫 1 KB string ≈ 1 µs
- 一次 list bounds check ≈ 0.5 µs per element
- 一次 dict layout walk ≈ 1 µs per field
- 一个典型 `#main(User, Cart)` 入参约 100 µs 验证开销

post-P2 wall time eval_steady/simple ≈ 22 µs。**100 µs 验证 vs 22 µs 真实
eval = wasm 后端的性能优势直接消失**。这违背决策 1 的根本动机。

## Implementation hints

### `BufferBuilder` 设计

```rust
// relon-eval-api/src/buffer.rs (Phase 0 输出，不在本 ADR 范围)

pub struct BufferBuilder<'a> {
    schema: &'a Schema,
    layout: OffsetTable,
    bytes: Vec<u8>,
    written_fields: HashSet<String>,  // 检测 missing field at finish
}

impl<'a> BufferBuilder<'a> {
    pub fn new(schema: &'a Schema) -> Self { ... }

    /// 写一个字段。T 必须匹配 schema field 的类型，否则编译期失败。
    pub fn write<T: BinaryEncodable>(
        &mut self,
        field: &str,
        value: T,
    ) -> Result<(), BufferError> {
        let field_layout = self.layout.field(field)
            .ok_or(BufferError::UnknownField)?;
        if field_layout.type_kind != T::KIND {
            return Err(BufferError::TypeMismatch {
                field: field.to_string(),
                expected: field_layout.type_kind,
                got: T::KIND,
            });
        }
        T::encode(value, &mut self.bytes, field_layout.offset)?;
        self.written_fields.insert(field.to_string());
        Ok(())
    }

    /// finish：检查所有字段都写过，返回最终 buffer
    pub fn finish(self) -> Result<Vec<u8>, BufferError> {
        for required in self.schema.required_fields() {
            if !self.written_fields.contains(required) {
                return Err(BufferError::MissingField {
                    field: required.to_string(),
                });
            }
        }
        Ok(self.bytes)
    }
}

pub trait BinaryEncodable {
    const KIND: TypeKind;
    fn encode(self, bytes: &mut Vec<u8>, offset: usize)
        -> Result<(), BufferError>;
}

impl BinaryEncodable for i64 { ... }
impl BinaryEncodable for f64 { ... }
impl BinaryEncodable for String { ... }
impl BinaryEncodable for bool { ... }
// ...
```

### Predicate 验证保持原路径

`#schema User { String name: where len(name) > 0 }` 这种 predicate：

- analyzer 把 predicate 关联到 schema field（已存在）
- wasm codegen emit 对应 predicate function call + branch on result
- 不通过 → wasm trap `SchemaPredicateFailed(field_idx)` → translate 为
  `RuntimeError::SchemaPredicateFailed { field, range }`

wasm bytecode 示例（伪代码）：

```wasm
;; after writing field "name" via run_main entry,
;; emit predicate check:
(local.get $name_ptr)
(call $check_name_len_gt_zero)  ;; user-defined or stdlib helper
(if (i32.eqz)
  (then
    (i32.const PREDICATE_FAILED_NAME)
    (call $trap_schema_predicate)
  )
)
```

### Host 端 predicate 重复验证（可选）

host SDK 可以**可选** 在 `BufferBuilder::write` 时执行 predicate
验证——这样错误更早暴露。但**默认不做**，因为：

1. predicate 可能很重（涉及 closure / 跨字段引用）
2. wasm 内部已经会 enforce，host 重复做是冗余开销
3. 真正 high-trust host（CLI、playground）不需要

host SDK 提供 `BufferBuilder::with_strict_predicates(true)` 让用户
opt-in 早期验证；默认 off。

## Consequences

正面：

- wasm 端零 layout validation 开销，决策 1 性能价值完整保留
- 用户感知的 schema 严格度跟 tree-walker 一致
- host SDK 边界类型安全 → 大部分 host bug 编译期被抓
- 风险半径限制在 wasm sandbox

负面：

- host SDK 必须做对 layout 编码——这是 typesafe API 的全部价值
- host SDK 维护成本高（每个新 type / schema feature 都要扩 BinaryEncodable
  impl）
- 跨语言 host SDK（如果未来要 Go / Java host）必须每语言重写
  BufferBuilder 等价物——这是 binary handshake 的固有代价

## 测试覆盖

Phase 2+ 实施时：

- `BufferBuilder::write::<i64>("name", 30)` 在 schema name 是 String
  时编译期失败 ✓
- 漏写 required field → `BufferBuilder::finish()` 报
  `MissingField`
- 越界 / 类型不一致的合法 byte sequence 喂给 wasm → wasm trap 但
  不溢出 host 进程
- predicate failure → `RuntimeError::SchemaPredicateFailed` 带正确
  TokenRange
- `BufferBuilder::with_strict_predicates(true)` 在 host 端早期捕获
  predicate fail

## 未来 v2 考虑

如果出现"不可信 host"场景（比如 wasm 模块被多租户加载，host 是
不可信端）：

- v2 引入 `WasmModule::with_layout_validation(true)` 开关
- wasm codegen 在 `run_main` 入口 emit 完整 layout scan
- 标记为高安全 / 性能下降模式

v1 不做。Relon 当前用户场景都是"host 是受控的"（CLI、自己的服务）。
