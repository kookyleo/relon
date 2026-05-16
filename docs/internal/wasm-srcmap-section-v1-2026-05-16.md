# WASM Custom Section srcmap v1（2026-05-16）

> Phase 0 子项 3/8。锁定 wasm 模块里**两个** custom section 的二进制
> 格式：
>
> 1. `"relon.srcmap"` — `pc → TokenRange` 映射（运行时错误回溯用）
> 2. `"relon.abi"` — ABI 版本 + schema hash（加载期 reject mismatch 用）
>
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> 决策 3 "wasm custom section 内嵌 srcmap"；[`wasm-binary-layout-v1`](./wasm-binary-layout-v1-2026-05-16.md)
> "ABI Version" 节。

## Section 1：`"relon.srcmap"`

### 整体结构

```
custom section payload:
  magic: [u8; 4] = b"RLNS"          ;; Relon Srcmap
  format_version: u8 = 1
  flags: u8                          ;; bit 0: compressed (always 0 in v1)
  file_count: varuint32
  file_table: [String; file_count]   ;; relative source paths
  entry_count: varuint32
  entries: [Entry; entry_count]       ;; sorted by pc ascending
```

### 编码细节

- **magic**：4 字节常量 `0x52 0x4C 0x4E 0x53`（"RLNS"），host SDK 检
  magic 不匹配就忽略 section（兼容未来变形或第三方 wasm 工具误伤）
- **varuint32**：标准 LEB128 unsigned 32-bit，与 wasm spec 一致
- **String**：`varuint32 byte_len` + `[u8; byte_len]` utf-8（不含 nul）

### `Entry` 结构

每条 entry 表达 "从 pc P0 起到下一条 entry pc 之前的所有指令都属于
该 source range"：

```
Entry:
  pc_delta:    varuint32   ;; 相对于上一条 entry 的 pc 差（首条 = absolute pc）
  file_idx:    varuint32   ;; index into file_table
  line:        varuint32   ;; 1-based
  col:         varuint32   ;; 1-based
  range_len:   varuint32   ;; source 字符数（不是字节）
```

**pc_delta** 用 delta 编码是 source map 通用做法，能把每条 entry
压缩到 ~5-15 字节。

### 查询语义

host runtime 查 `pc → range`：

1. 二分查找 entries（已按 pc 排序）
2. 找到 last entry with `entry.pc <= query_pc`
3. range 是 `TokenRange { file: entry.file_idx, line: entry.line, col:
   entry.col, length: entry.range_len }`
4. 若 query_pc 落在最后一条 entry 之外（pc > last_entry.pc + 模块代码
   段长）→ 报 "unknown pc"，错误信息退化为 wasm raw trap

### 体积预算

实测 / 估算：

- 每条 entry 平均 ~10 bytes（varint 压缩后）
- 每条 wasm 指令大约对应 1 条 entry（粗粒度），细粒度可以一条 entry
  跨 5-20 条指令
- 典型 10 KB code section 对应 ~500-2000 entries → srcmap ≈ 5-20 KB
- **预算上限**：srcmap section size ≤ 30% of code section size
- 超过时 codegen warn 但不 fail；用户可以选择 `--strip-srcmap` 在
  release build 关掉

### 工具兼容性

- `wasm-opt` 默认保留 unknown custom section（验证过）
- `wasm-validate` 不解析 unknown section，直接 skip
- `wasm-tools strip` 命令可以**显式 strip** —— 是 release build 优化
  入口
- 浏览器 / wasmtime 加载时 ignore unknown custom section，零运行时
  开销

## Section 2：`"relon.abi"`

### 结构

```
custom section payload:
  magic: [u8; 4] = b"RLNA"            ;; Relon Abi
  format_version: u8 = 1
  abi_version: u16 LE                 ;; bump on breaking layout change
  codegen_version: u32 LE             ;; bump on any codegen change (advisory)
  main_schema_hash: [u8; 32]          ;; sha256 of canonical #main schema
  return_schema_hash: [u8; 32]        ;; sha256 of canonical return type
  flags: u8                            ;; bit 0: sandboxed
                                       ;; bit 1: dhat-trace embedded（v2+）
```

### 加载期检查

Host SDK instantiate 流程：

```
1. read "relon.abi" section
2. if not present  → refuse-to-load (RuntimeError::AbiSectionMissing)
3. if magic != RLNA → refuse (Corrupted)
4. if format_version != 1 → refuse (FutureFormat)
5. if abi_version != SDK_EXPECTED_ABI → refuse (AbiMismatch { wanted, got })
6. compute host-side schema hash from compile-time #main schema
7. if main_schema_hash != computed → refuse (SchemaDrift)
8. instantiate
```

`SchemaDrift` 防止 host 用旧 schema 写 binary buffer，wasm 是按新
schema 编出的——layout 错位后果非常隐蔽（wasm 读出垃圾数据），所以
**必须** schema hash check 防御。

### `canonical #main schema` 定义

为了让 host 和 wasm codegen 算出**相同的** hash，"canonical" 形式必须
确定性 serialize：

```
canonical_schema(Schema) = {
    "version": 1,
    "name": <Schema.name>,
    "generics": [...],
    "fields": [
        { "name": <field_name>,
          "type": canonical_type(field.type_hint),
          "default": canonical_value(default) if any
        }, ...
    ],
}

sorted by field declaration order (NOT alphabetical).
```

- `canonical_type` 同样递归 canonical
- Schema 引用其它 Schema 时 inline 展开
- doc comments / decorator metadata **不进** canonical form

序列化用 `serde_json` with sorted keys + no whitespace；hash 用 sha256
取 hex 字符串前 32 bytes。

### 多文件场景

`canonical_schema` 不带文件路径——一个 schema 不论在 `lib/foo.relon`
还是 `lib/bar.relon` 里定义，只要结构一样，hash 就一样。这是"行为
hash"，不是"位置 hash"。

`#import "lib/foo"` 引入的 schema 在 hash 计算时 inline 展开。

## codegen pipeline 集成

```
relon-ir IR ops carry TokenRange + file_idx
                │
                ↓
relon-codegen-wasm:
  - emit code bytes
  - parallel: collect (pc, range) pairs
                │
                ↓
  finalize:
    - sort entries by pc
    - delta-encode pc
    - varint encode all
    - append as custom section "relon.srcmap"
    - compute schema hash + abi metadata
    - append as custom section "relon.abi"
                │
                ↓
output .wasm
```

## host runtime API（草案）

```rust
// crates/relon-codegen-wasm/src/runtime.rs（暂定位置）

pub struct WasmModule {
    bytes: Vec<u8>,
    srcmap: SrcMap,           // 解析自 "relon.srcmap"
    abi: AbiMetadata,         // 解析自 "relon.abi"
}

impl WasmModule {
    pub fn from_bytes(b: Vec<u8>) -> Result<Self, AbiError> { ... }

    pub fn translate_trap(
        &self,
        trap: wasmtime::Trap,
    ) -> RuntimeError {
        let pc = trap.bytecode_offset();
        let kind = trap.trap_code();
        let range = self.srcmap.lookup(pc);
        RuntimeError::from_trap_kind(kind, range)
    }
}
```

`translate_trap` 把 wasmtime 的 `(pc, trap_code)` 翻译回与 tree-walker
等价形状的 `RuntimeError { range, ... }`，让 `Box<dyn Evaluator>`
host 完全感知不到 backend 差异。

## 兼容性

- v1 → v2：`abi_version` bump，magic 不变。Host SDK 同时支持多个
  abi_version 时 fan-out
- 任何破坏 binary layout 的修改必须 bump `abi_version`
- 仅扩展（add field、新 trap kind）可以 bump `codegen_version`，
  `abi_version` 保持

## Phase 0 checklist 推导

- [ ] `relon-codegen-wasm` 内 `srcmap.rs`：emit + parse
- [ ] `relon-codegen-wasm` 内 `abi.rs`：hash 计算 + section emit + parse
- [ ] `relon-eval-api` 内 `SchemaCanonical` helper（为了让 host 跟
  wasm 算同样的 hash）
- [ ] integration test：roundtrip srcmap、abi mismatch refuse-to-load
