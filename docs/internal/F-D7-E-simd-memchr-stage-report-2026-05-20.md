# F-D7-E stage report — SIMD memchr for needle=1 `StrContains`

> **Date**: 2026-05-20
>
> **Base HEAD**: `3d7b614 merge(trace-jit): F-D8-E.2 DictLookup IC inline (shape-compare hoist)`
>
> **Scope**: upgrade `relon-trace-emitter::str_inline::emit_scan_single_byte`
> from a scalar byte-at-a-time loop to a `memchr`-style 16-byte SIMD
> chunked scan with a scalar tail.

## 一、改动文件

| 文件 | LoC | 说明 |
|---|---|---|
| `crates/relon-trace-emitter/src/str_inline.rs` | +118 / -28 | module 头部 SIMD 说明替换原 "Why scalar, not SIMD" 段；`emit_scan_single_byte` 重写 — splat I8X16 + 16-byte chunk loop（`icmp eq` lanewise + `vhigh_bits → I16` 取 bitmask + `icmp_imm ne 0` early-exit）+ 标量 tail loop（≤ 15 byte） |
| `crates/relon-codegen-native/tests/str_contains_inline_exec.rs` | +71 / -0 | 新增 `inline_one_byte_simd_chunk_hit_positions`（覆盖 15/16/17/31/32/33/256/512/1024 byte haystack，hit 位置跨 chunk 边界）+ `inline_one_byte_simd_empty_and_short_haystacks`（0..15 byte tail-only） |

## 二、SIMD lowering 形状

cranelift 0.131 portable v128 ops，由 backend lower 为 native：

- **x86_64 SSE2**：`load v128` → `pcmpeqb` → `pmovmskb` → `test r16, r16` →
  `jnz`. 标量 tail 仍是 `movzx + cmp + je`.
- **aarch64 NEON**：`ld1.16b` → `cmeq.16b` → `umaxv` / `shrn` → 标量
  mask → `cbnz`.

每 16 byte 一次循环体：1 v128-load + 1 lanewise compare + 1 mask extract +
1 conditional branch + 1 cursor += 16 + 1 backedge jump. Tail 走原 scalar 形状。

`splat(I8X16, needle)` 在 loop preheader 里 emit，cranelift egraph 会把它
hoist 到入口寄存器；`chunk_end = h_ptr + (h_len & !15)` 与
`end_ptr = h_ptr + h_len` 一并在 preheader 一次性算好。

## 三、测试覆盖

```
running 8 tests
test inline_empty_needle_is_always_hit ... ok
test inline_eight_byte_needle_matches_extern ... ok
test inline_needle_at_end_of_haystack_is_a_hit ... ok
test inline_one_byte_needle_matches_extern ... ok
test inline_one_byte_simd_chunk_hit_positions ... ok
test inline_one_byte_simd_empty_and_short_haystacks ... ok
test inline_repeated_match_short_circuits_correctly ... ok
test inline_sixteen_byte_needle_matches_extern ... ok
```

新 SIMD path 在 256 / 512 / 1024 byte haystack 上 byte-identical 于
extern `__relon_str_contains` 参考；0..15 byte 边界（chunk loop 不进入 →
直跳 tail）覆盖 empty haystack / single-byte hit / mid-haystack hit / 末
位 hit 全过。

## 四、W4 cmp_lua bench

| Path | Before (3d7b614) | After (this stage) |
|---|---|---|
| `relon_trace_jit` | 29.670 µs | 29.707 µs |
| `luajit` | 17.890 µs | 17.929 µs |
| **ratio trace_jit / luajit** | **× 1.658** | **× 1.657** |

**目标 × 1.3 未达**。差值在 noise 内（criterion `change`: +0.0014 % ~
+0.19 %, "No change in performance detected"）。

诚实记录原因：

- W4 fixture 的 haystack 是字面量 `"axb"` — 3 byte. SIMD 16-byte chunk
  path 永远不会进入；entry block 立刻判定 `cursor == chunk_end`（因为
  `h_len & !15 == 0`）后跳到标量 tail。
- 标量 tail 与 F-D7-C 之前的 scalar `memchr` 形状一致 — 每 iter 同样的
  load + icmp + brif + iadd. W4 hot loop 的 `contains("axb", "x")` 在标
  量 path 上只做 2 次 byte compare 就 hit. 这里 SIMD 没有任何加速面。
- W4 trace_jit / luajit 真正剩余的 1.66× gap 不在 byte-scan 上，而在外
  层 — 每 iter `load (ptr, len)` from `*const StringRef`（cranelift
  0.131 无 LICM 把 const-haystack 的 deref 提出），加 `bool → i64`
  extend，加 `count += hit / i += 1` 的 SSA pair。
- F-D7-G（并行 agent）正好在做 LICM hoist of StringRef payload load
  (`offsets 0/8`)，本地 main 已合入但本 worktree 基于 `3d7b614` 没引入。
  F-D7-G + F-D7-E 合流后才是 W4 的最终形态：preheader 单次 deref + 内
  层 SIMD scan + tail，inner loop body 缩小到只剩 cursor 步进与
  count++。

## 五、Gate

```
cargo build --workspace                   # ok
cargo test --workspace                    # ok (str_contains_inline_exec: 8 passed)
cargo clippy --workspace --all-targets    # ok (-D warnings clean)
cargo fmt --all -- --check                # ok
cargo build --target wasm32-unknown-unknown -p relon-wasm  # ok
cargo run -q -p relon-fmt -- --check ...  # ok
```

## 六、Remaining todo

- W4 ratio × 1.3 目标实际依赖 F-D7-G LICM 落地后的合流验证；本 stage
  落 SIMD lowering 后，等 F-D7-G merge 再跑一次 W4 bench 即可确认。
- 长 haystack（≥ 16 byte）的 W4-shape bench fixture 是新单独价值的覆盖
  面，可加 `W4b_string_contains_long`，专门给 SIMD path 留观测窗。本 stage
  不引入新 bench fixture（scope creep）。
