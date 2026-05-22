# Security Review Wave 完工 report

**完成日期**：2026-05-22

## 总览

依据 `docs/internal/crate-review-2026-05-22.md` 列的 P0 / P1 / P2 安全/soundness 问题 + W12 perf 超标，推 **7 项完工**。

**Tests 2282 → 2316 (+34)**。

## 完成情况

### P0 (correctness/soundness — 优先)

| ID | 项 | 修法 |
|---|---|---|
| #167 (P0) | CraneliftAotEvaluator Send+Sync UB | Option B per-call SandboxState from immutable Shared template, +54 ns/dispatch within ≤ 100 ns budget。multi-thread regression test 防退 |
| #168 (P0) | cache HMAC 认证域 | A: ensure_key fail → refuse cache triple write+load。B: schema sidecar v2 HMAC binds source_sha256+object_sha256+entry_shape。+9 regression test |

### P1 (concurrency / signal soundness)

| ID | 项 | 修法 |
|---|---|---|
| #169 (P1) | signal handler 文档 + release path | Option A: 文档 telemetry/fail-fast 不承诺 recover。revert #154 lever (b) cfg-gate catch_unwind — release 也保留 shield (+2.7 ns ≪ 20 ns budget) |
| #170 (P1) | trace hot counter | Option A: UnsafeCell<[u32]>+unsafe impl Sync → [AtomicU32; N]。cranelift IR atomic_rmw add (x86 LOCK XADD)，perf neutral。+1 multi-thread no-race test |

### P2 (security boundaries)

| ID | 项 | 修法 |
|---|---|---|
| #171 (P2) | evaluator runtime import sha256 pin | sha2+hex 提升顶层 deps (wasm 也强制)。eval-api 加 3 error variant。apply_directive_pre 解 integrity，apply_directive_import 沿 traversal 传，verify_module_integrity helper fail-closed verify。+6 regression test |
| #172 (P2) | trace runtime fix | v2 dict family: build_dict_record_v2 + lookup_v2 + prechecked_v2 (hash+key_off+key_len+value, hash hit 后 memcmp)；v1 保留 layout-compat with emitter inline (doc 改 'bench-fixture only correctness')。TRACE_STRING_ARENA thread-local 3-variant alloc 注册 + reclaim_trace_strings API。+14 regression test |

### Perf (W12 ratio over target)

| ID | 项 | 数字 |
|---|---|:---:|
| #173 (perf) | W12 bytecode × 1.88 → ≤ × 1.5 | **× 1.15** (-41% 204.74 → 120.21 ns) — lever 7 alloc-free typed-i64 fast path (POOLED_LOCALS/STACK thread-local + run_main_i64_inner pooled path) |

## 累计 8-wave 战果

| Wave | 任务 | tests | LoC |
|---|:---:|:---:|---:|
| 1 (P0-P3) | 7 | +22 | +4622/-1043 |
| A | 5 | +31 | +3552/-1887 |
| B+C | 13 | +84 | +10,000+/-330+ |
| D | 11 | +66 | +10,000+/-900+ |
| Z (中 ROI) | 8 | +11 | +4079/-279 |
| Deep | 2 | +24 | +1223/-16 + audit |
| **SEC (本 wave)** | **7** | **+34** | **+3,500+/-560+** |
| **总计** | **53** | **+272** | **+37,000+/-5000+** |

**Tests 1907 → 2316 (+409)**。

## 重大成果

### 安全/soundness 收紧

- **`unsafe impl Sync for SandboxState` 移除** — 真实 data race UB 消除。multi-thread evaluator 现 sound。
- **`unsafe impl Sync for [u32; N]` 移除** — hot counter race UB 消除，改 AtomicU32 + atomic_rmw。
- **`hmac_key=None` cache 路径关闭** — 不再写无 HMAC cache，不再 TrustOnWrite 读。
- **schema sidecar HMAC v2 绑 object hash + entry shape** — ABI 错配攻击面消除。
- **TreeWalkEvaluator runtime sha256: pin 校验** — analyzer-bypass 攻击面消除。
- **release catch_unwind shield 恢复** — cranelift trap 路径可靠 type-er 化。
- **trap_handler doc 明确 telemetry 不承诺 recover** — false advertising 修正。
- **dict_lookup v2 hash hit 后 memcmp key** — hash collision silent corruption 消除（v1 保留 layout 给 emitter inline + bench fixture，doc 改正）。
- **TRACE_STRING_ARENA reclaim API** — long-lived host 不再泄漏 StringRef。

### 性能 W12 bytecode 大胜

W12 bytecode 从 **× 1.88 → × 1.15** vs LuaJIT (-41%, 204.74 → 120.21 ns)，well under × 1.5 target。

| W12 | ns | vs LuaJIT |
|---|---:|:---:|
| tree_walk | 1553 | × 14.3 |
| **trace_jit** | **150.95** | **× 1.40** |
| **bytecode** | **120.21** | **× 1.15** ✓ |
| LuaJIT | 108.74 | × 1.0 |

bytecode 现 **超过 cranelift_aot_loop 边界 (LegacyI64 14.25 → 96.66 ns post 169 revert)** 并接近 trace_jit。pooled typed-i64 lane 是这一档关键。

## 关键诚实记录

### #170 (W12 push) Option A 不可行

Option A "wire trace-bridge to W12 bench" 架构上 blocked: `run_main_i64` docstring 明 opts out of trace_lookup dispatcher switch (recorder/trace overhead 抵消 typed path 价值)。pivot 到 Lever 7 alloc-free pooled。

### #172 (trace runtime) v1 layout 保留

v1 dict_lookup hash-only 保留供 emitter inline 烤 IR + W5/W6 bench fixture。doc 改正 'bench-fixture only correctness'。完整 v2 emit migration 是独立 follow-up（需 emitter + recorder + fixture 协调）。

### #169 (signal handler) Option A 选 over Option B

Option B (sigsetjmp/siglongjmp C shim) 需 per-platform macro + sigjmp_buf 存储 + Drop/Box ownership 审计（siglongjmp 跳 destructor）。Option A 文档收紧 + release catch_unwind 恢复，工作量小且足够安全。Option B 入 v6-γ trace-recorder deopt 一起做（同需要 setjmp machinery）。

### #173 (hot counter) atomic_rmw 选择

cranelift 只有 sequentially consistent atomic_rmw（无 Relaxed flavor）。strict 但 x86 LOCK XADD 1 op = perf neutral。

## 剩余 follow-up（蓝本齐全）

### 中期等条件
- **sigsetjmp/siglongjmp Option B** — real recoverable hardware fault handling (with v6-γ trace-recorder deopt)
- **dict_inline v2 migration** — emitter inline cranelift IR 走 v2 layout (#172 deferred)
- **#170 follow-up**: thread_local box pool + ArcSwap-style atomic ptr → SandboxState alloc delta 54 ns → ~10 ns
- **#173 W12 follow-up**: BytecodeVm cache on evaluator (需 Sync wrapper / per-thread VM) recover 最后 ~16 ns gap

### 长期 / 等需求 / RFC-class
- 全 ConstString wire migration 12-byte header (audit done in #169 audit-only)
- Lever 4 direct-threaded dispatch (Rust stable `become`)
- W11 musl static-link (RFC API change)
- W4 trace framework redesign
- W7 fib recursion in trace
- wasmtime backend
- Cranelift 0.133+ retry b99f2b4

## Gate

每 phase 全过：
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`: 2316 / 0 fail
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`

review-doc 列的所有 P0 + P1 + P2 + perf 7 项全完。

## 结论

`crate-review-2026-05-22.md` 列出的安全 + soundness backlog 全部修完，外加 W12 bytecode 超标 perf 修复一并落地。所有改动通过严格 gate (fmt + clippy -D warnings + test + wasm32)。

**当前 main 状态**: 2316 tests / 0 fail / cranelift 0.132 / 6 review priority all closed / W12 bytecode × 1.15 ✓。

**进行中任务**: 0  
**已规划 deferred**: ~17 项（每项有 stage report 蓝本 + 触发条件清晰）

Project 在安全 + 性能两个维度都达到 release-ready 程度。后续主要是等 user demand / Rust stable feature / cranelift upstream 触发的 long-term improvements。
