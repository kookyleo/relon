# Review-Improvement #149 — String Tier 1a: cached fx_hash in key-record header

## Scope

Tier 1a 优化：把每个 dict-key record 的 fx_hash 预算放进 header，热路径
load.u64 一次替代字节级 hash 循环。LuaJIT GCstr 头存 hash 的常见做法。

## Layout before / after

Producer 端 helper `build_string_record(s: &str) -> Vec<u8>`：

```text
before (header = 4 bytes)
  offset 0  : len   : u32 LE
  offset 4  : bytes : [u8; len]

after  (header = 12 bytes)
  offset 0  : len   : u32 LE
  offset 4  : hash  : u64 LE     (pre-computed fx_hash_bytes(payload))
  offset 12 : bytes : [u8; len]
```

`STRING_RECORD_HASH_OFFSET = 4` 与 `STRING_RECORD_PAYLOAD_OFFSET = 12`
作 `pub const` 钉死在 `relon-trace-abi::hash`。Producer 和 consumer
（dict_inline 的 cranelift IR）共用同一组常量，drift 即编译期 catch。

`fx_hash_key_record(key_ptr)` 改成单条 `load.u64 [key_ptr + 4]`；
`fx_hash_key_record_payload` 保留字节级 reference 给 producer-side
stamp + tests round-trip。

## Compile-time hash 范围

本轮 Tier 1a 实施只覆盖 **dict-key records**（`build_string_record`
产出的 12-byte-header 格式）。General `Op::ConstString` 还走原 4-byte
header，因为 ConstString 输出被 `Op::ReadStringLen` / `Op::LoadStringPtr`
等多处消费者按 offset 4 读 payload，全面迁移代价远超 Tier 1a 预算。

W5 bench fixture 的 key_records 已自动跟随 `build_string_record` 走
新格式；trace-recorder + JIT 路径 100% 走 cached hash。

## W5 dict_str_key bench number before / after

| | trace_jit | LuaJIT | ratio |
| --- | ---: | ---: | ---: |
| F-D8-E.7 baseline (last stage report) | 168.52 µs | 114.38 µs | × 1.473 |
| **#149 Tier 1a (this stage)** | **131.08 µs** | 113.84 µs | **× 1.151** |

trace_jit 改善 `(168.52 - 131.08) / 168.52 = 22.2%` —— 大幅超过任务
预期的 −10~15%。LuaJIT baseline 在同机重测一致。ratio 从 × 1.473 缩到
× 1.151，超过 × 1.30 目标。

`cmp_lua_dict_list_trace` bench rerun 在 121.54 µs，相对 baseline
−27.9%（两个 bench 入口走的 setup 略不同；前者直接调 inline emitter，
后者经 recorder pipeline）。

## 哪些 op 用 cached / 哪些还现算

**Cached hash（load.u64 at offset 4）**：
- `__relon_trace_dict_lookup`（runtime helper）
- `__relon_trace_dict_lookup_prechecked`（prechecked variant）
- `emit_dict_lookup_inline_with_hoists`（data-driven inline emitter）
- `emit_dict_lookup_inline_unrolled`（fully-unrolled inline emitter）

**还现算（byte-wise FxHash）**：
- `fx_hash_bytes` 本身（producer 端 stamp 时调用一次）
- `shape_hash_for_keys`（IR 端 stamp Op::DictGetByStringKey::shape_hash）
- `fx_hash_key_record_payload`（test-only fallback）

**未触及（保持 4-byte header）**：
- `Op::ConstString` const-pool 输出
- `Op::ReadStringLen` / `Op::LoadStringPtr` 等 wasm-arena 消费者
- `__relon_str_concat_alloc` 等 StringRef 路径（payload 由 JIT
  写入，没有进入 dict-key 数据流）

`DictInlineHoists.hash_seed` / `hash_prime` 字段保留在公共 surface
（callers 仍 populate），但不再驱动任何 per-iter IR；后续可顺手 drop。

## LoC delta

`git diff --stat d8ef62a..HEAD`：7 files changed, **+214 / -186**，
净增 +28 行（dict_inline.rs hash 循环删除抵消了 hash.rs / dict_list.rs
的注释扩充 + 新 helper）。

## Gate

- `cargo fmt --all --check` 干净
- `cargo clippy --workspace --all-targets -- -D warnings` 干净
- `cargo test --workspace` **2161 passed / 0 failed**
- `cargo check --target wasm32-unknown-unknown -p relon-wasm` 干净
- W5/W6 recorder trace tests pass
- `cmp_lua_consistency` 10 cases 全 pass（含 W3 / W4 / W5 / W6）

## Commits（agent worktree branch `worktree-agent-ae42ad0e64c503c30`）

1. `7838cdc refactor(trace-abi): widen dict key record header to include cached fx_hash`
2. `86b3c54 refactor(trace-emitter): dict_inline loads cached fx_hash from key record`
3. `0d48f1c test(ir): update shape_hash round-trip test to new key record layout`

## Follow-ups（不在 Tier 1a scope）

- **Incremental hash update for str_concat**：`__relon_str_concat_alloc`
  目前不写 hash field；若未来 concat 结果会进 dict-key，需要 concat
  完后跑一次 fx_hash 或增量更新。
- **General ConstString migration**：把 12-byte-header 推到所有
  string-record，需同步迁移 `Op::ReadStringLen` / `Op::LoadStringPtr`
  及 codegen-native const_pool。当前 ConstString 输出不进 dict-key
  数据流，所以不阻塞 Tier 1a。
- **Drop `hash_seed` / `hash_prime` 字段**：等 emitter.rs 的 preheader
  emit 同步 stop populating，DictInlineHoists 可瘦身为 1 字段。
