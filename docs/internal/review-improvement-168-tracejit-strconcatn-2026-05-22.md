# #168 trace-JIT 接入 Op::StrConcatN — stage report 2026-05-22

## 范围

历史上 `Op::StrConcatN` 在 trace-recorder 落 `AbortReason::UnsupportedOp("StrConcatN")` sticky abort，hot loop 一旦含 ≥ 3 string concat chain，整段 trace 进 cranelift AOT fallback。本轮把 N ≤ 4 的 chain 接入 trace-JIT inline path，N > 4 仍 abort（依任务"诚实"约定，可接受）。

## 实现

1. `TraceOp::StrConcatN { dst, operands: Vec<SsaVar> }` 新变体落 trace-jit IR，effect_class = `Pure`（与 2-operand `StrConcat` 对齐，LICM / const-fold / dead-store 一致处理）。`output()` / `inputs()` / `defs()` 与 `load_forward` 的 `swap!` 循环都按 variable-arity 处理每个 operand SSA slot。

2. recorder `lower_str_concat_n`：移除 abort，pop N inputs（top-first），反转入源序，emit `TraceOp::StrConcatN` + N 个 `NotNull` guards。MAX_INLINE_STR_CONCAT_N = 4 cap 落 recorder 侧；`operand_count < 3` / `> 4` 分别 abort 出独立 diagnostic。

3. trace-recording walker 增 `step_str_concat_n`：pop 精确 N cells（避免 catch-all 全 stack forward 导致 `apply_outcome` 过度 pop），chain `__relon_str_concat` 左→右计算 recording-time value。

4. emitter `emit_str_concat_n`：load 每个 operand `.len` → `iadd` 累加 total_len，stack-spill 操作数指针入 `[*const StringRef; N]` slot，一次性 call `__relon_str_concat_n_alloc(slot_ptr, N, total_len)`，followed by `seal_hash` call（Tier 1b 对齐）。新 runtime helper 单 `[header | payload]` block，sequential per-operand memcpy。

5. HostHookId::`StrConcatNAlloc` + HostHookFuncIds::`str_concat_n_alloc`，opt-in；缺 helper 时 emit-time `HostHookNotDeclared` 干净 fallback。`inline_emit` 走 `CallNotSupportedInInline`（与 sibling Str* op 一致）。

6. host wiring：`trace_install` 声明符号 + register through JITBuilder。

## 测试

- trace_ir: 2 个新 variant test
- runtime: 7 个 alloc helper test（含 hash seal parity）
- recorder lowering: 5 个 lower_op test（含 cap / underflow / source-order）
- recorder integration (record_str_concat_n.rs): 3 test 全 RecorderState 链路
- emitter (emit_str_ops.rs): 3 个 IR-level test
- e2e (str_concat_n_trace_exec.rs): 4 个 TraceJitState install + invoke，含 hot loop 32 iter

## Gate

`cargo fmt --all --check` 干净；`cargo clippy --workspace --all-targets -- -D warnings` 0 warning；`cargo test --workspace` 2282 passed 0 failed（≥ 2258 gate 满足）；`cargo check --target wasm32-unknown-unknown -p relon-wasm` 通过。2-operand `TraceOp::StrConcat` regression test（emit_str_ops.rs 内 4 个 inline / extern 案例）全过。

## Branch / Commits

Branch: `worktree-agent-a3672b333a308d230`
HEAD: `864de54af3c679254753e7d692f2414daece530f`

6 commits（按 IR → recorder → emitter → host → tests → fmt 顺序）：
- `967fa6e` refactor(trace-jit): TraceOp::StrConcatN + effect_class + alloc helper
- `110789f` feat(trace-recorder): lower Op::StrConcatN onto TraceOp::StrConcatN
- `cd8a7a1` feat(trace-emitter): emit_str_concat_n_inline_unrolled N <= 4
- `0779ca8` feat(codegen-native): wire Op::StrConcatN through walker + hook
- `5ac0882` test(trace-jit): StrConcatN e2e + recorder integration
- `864de54` style: cargo fmt cleanup for StrConcatN additions

## 边界 / 诚实

- N > 4 仍 abort（`UnsupportedOp("StrConcatNOverCap")`）；cap 单独常量 `MAX_INLINE_STR_CONCAT_N`，未来扩展只改一处。
- `inline_emit` 暂留 `CallNotSupportedInInline`；与 `StrConcat` / `StrContains` 等 sibling 一致。
- 未 push 远端。
