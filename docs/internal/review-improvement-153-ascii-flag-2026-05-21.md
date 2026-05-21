# #153 String Tier 2c — ASCII flag bit on StringRef header

## Layout 选 (C) + 理由

复用 `len: u32` 高位作 ASCII flag。Header 仍是 12 字节，`[len_with_flags: u32][hash: u64][payload]`，没有 offset / stride 移动。

- (A) 偷一位 hash bit：缩小 IC 有效 hash space，对 W5 corpus 的 collision rate 有量化恶化，被否决。
- (B) 加 `flags: u32`：header 长到 16 字节，dict_inline 的 hash offset / scan stride / 全部 bench fixture 都要同步改，全部 LE 序列化的 layout-smoke 测试也要更新。改动面太大。
- (C) 复用 len 高位：interning 表内每个 dict key 都远小于 2^31 字节（实际 <100 字节）。一次 `& STRING_RECORD_LEN_MASK` 读出真实长度，一次 `& STRING_RECORD_ASCII_FLAG_BIT` 探测 flag。LuaJIT GCstr 用同样 hash24/len24/ascii8 split，模式有先例。

Const 约定：`build_string_record` debug 模式 panic on `len >= 2^31`；release 时高位会被覆盖，ASCII flag 失真，所以拉个 invariant 锁住。

## is_ascii_bytes 实施位置

- 声明：`crates/relon-trace-abi/src/hash.rs:215` `pub fn is_ascii_bytes(payload: &[u8]) -> bool`
- 调用点：`crates/relon-trace-jit/src/runtime/dict_list.rs:308` `build_string_record` 每次构造时调一次

实现是 `payload.iter().all(|b| *b < 0x80)`，LLVM auto-vec 到 SSE2 `pmovmskb`。100 字节 payload < 30 ns。

## Fast path 触发点列表

| LoC | 函数 | 调用点 |
| --- | --- | --- |
| `crates/relon-trace-abi/src/hash.rs:194` | `is_ascii_flag_set(key_ptr) -> bool` | 消费者侧 flag 探测 |
| `crates/relon-trace-abi/src/hash.rs:215` | `is_ascii_bytes(payload) -> bool` | 生产者侧分类 |
| `crates/relon-ir/src/unicode/ascii_fold_simd.rs:412` | `case_fold_ascii_fast(bytes, mode, ...)` | 跳过 scan 的核心 fast path |
| `crates/relon-ir/src/unicode/ascii_fold_simd.rs:454` | `case_fold_ascii_fast_into_string(...)` | `String` 接收方便方法 |
| `crates/relon-evaluator/src/stdlib.rs:917` | `fold_string_with_ascii_hint(s, mode, locale_turkish, AsciiHint)` | 接受外部 ASCII 分类提示的 entry point |
| `crates/relon-bench/benches/ascii_case_fold.rs:107` | `pre_classified_fold_string` | bench micro-row |

`fold_string` 表面 body （surface bodies — `upper` / `lower` / `title` / locale 变体）当前总传 `AsciiHint::Unknown` —— 把 Value→StringRef 的 flag bit 真正接到 evaluator 数据流上，留给后续 PR 做（涉及 evaluator 内部 string 容器，超出 #153 范围）。

## Bench 数字 (x86_64-v3 native, criterion --quick)

| 模式 | 大小 | baseline | simd (现有) | preclassified (新) | preclassified vs simd |
| --- | --- | --- | --- | --- | --- |
| upper | 1024  | 18.24 µs | 1.14 µs  | 0.14 µs | **-88 %** |
| lower | 1024  | 18.25 µs | 1.12 µs  | 0.15 µs | **-86 %** |
| title | 1024  | 19.63 µs | 4.43 µs  | 3.45 µs | -22 % |
| upper | 10240 | 182.64 µs | 10.85 µs | 1.40 µs | **-87 %** |
| lower | 10240 | 182.66 µs | 10.88 µs | 1.48 µs | **-86 %** |
| title | 10240 | 195.81 µs | 43.92 µs | 34.23 µs | -22 % |

Upper / Lower 远超 -30~50 % 目标。Title 因为 word-state walker 是 byte-wise 串行，scan 节省占总耗时比例小，所以只有 -22 %。后续考虑给 Title walker 加 SIMD 化或专门 LICM 是单独的 #154+ 议题。

## 验证 correctness

1. `relon-trace-abi`：14 tests pass (4 新增 ASCII flag 单元测试 — overlap / classifier / round-trip pure ASCII / round-trip non-ASCII)
2. `relon-trace-jit`：90 tests pass (4 新增 — build_string_record 对纯 ASCII / 非 ASCII / 空 / dict-lookup 与 flag 共存)
3. `relon-ir::unicode::ascii_fold_simd`：21 tests pass (4 新增 — fast path 与 scan path byte-identical / into_string 拼接 / empty 短路 / Title 携带 word state)
4. `relon-evaluator::stdlib::ascii_hint_tests`：2 tests pass (Unknown vs AllAscii vs KnownNonAscii parity 全模式覆盖)
5. 整个 workspace：**2210 tests pass** (高于 #153 基线 2196)
6. `cargo fmt --all --check` 通过
7. `cargo clippy --workspace --all-targets -- -D warnings` 通过
8. `cargo check --target wasm32-unknown-unknown -p relon-ir -p relon-trace-abi` 通过

Layout-smoke：dict-lookup 路径用 ASCII flag-bit 的 record 后仍命中（`dict_lookup_still_hits_with_ascii_flag_present`），证明 cached hash 与 flag bit 互不干扰（hash 只盖 payload bytes，不含 header）。
