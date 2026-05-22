# Cranelift loop-opt 退化排查 + 修复 + follow-up

**日期**：2026-05-22  
**触发**：全 bench 跑 verification 时发现 `cranelift_aot_loop` 从 2.07 → 4.33 ns/iter (+109%)

## TL;DR

- **Root cause**：`b99f2b4`（#136 lever 3 vDSO clock elide）把 `emit_resource_check` 从直线 IR 改成 brif-guarded shape，加 2 个 extra basic block。
- **影响**：cranelift loop-opt 启发式对 function-level basic block 总数敏感，多出的 block 把外层 `Op::Loop` 优化打了对折（per-iter cost 2.07 → 4.33 ns）。
- **修法**：`emit_resource_check` 回到直线 IR；vDSO elide 价值由 `now_helper` 自身 fast-return 保留（host-side sentinel check）。
- **Cranelift 0.132 升级**：试过，loop-opt 启发式没改，但 baseline 干净所以保留。
- **未取的 Option E**（双 variant 编译）：代价 2× cold start，break-even 需 87 万次 dispatch 才回本，对 Relon config-DSL 用例不值得。**记 follow-up，留待 HFT-类场景报需求**。

## 排查路径

### Bisect

机器：Xeon E5-2609 v4 @ 1.7 GHz max, schedutil, load ~3。

| Commit | cranelift_aot_loop | 状态 |
|---|---:|:---:|
| `c55bfb3` (Wave A end) | 2.07 ms | good |
| `936cd41` (#134 codegen split) | 2.07 ms | good |
| `75783b6` (#136 lever 1+2 typed-i64 + signal-handler lift) | 2.07 ms | good |
| **`b99f2b4` (#136 lever 3 vDSO elide)** | **4.35 ms** | **bad ← regression here** |
| `caf6416` (#136 merge) | 4.35 ms | bad |
| `069cc32` (#143 phase 4c merge) | 4.35 ms | bad |
| `9e808d8` (Wave D end) | 4.33 ms | bad |

### 失败的 split fix

试过把 `emit_resource_check` 拆两版：
- `emit_resource_check_with_skip`：prologue 用（保留 sentinel skip）
- `emit_resource_check_always`：back-edge 用（无 sentinel skip，原 shape）

预期：back-edge 是 hot path，回到原 shape 应 recover。**结果：仍 4.35 ms 没有 recover**。

结论：cranelift loop-opt 启发式看的是**函数全局 basic block 总数**，不只 back-edge 路径。prologue 的 +2 个 blocks 也参与决定。

### 完整 revert b99f2b4

立即回 2.07 ms。证实 b99f2b4 唯一 regress 点。

## Option I 精准修复（保留 lever 3 价值）

将 sentinel check 从 cranelift IR 移到 host helper：

```rust
// crates/relon-codegen-native/src/sandbox.rs
pub(crate) unsafe extern "C" fn now_helper(state: *const SandboxState) -> i64 {
    let state = unsafe { &*state };
    if state.deadline_ns.load(Ordering::Relaxed) == i64::MAX {
        return 0;  // fast-return, no vDSO
    }
    state.epoch.elapsed().as_nanos() as i64
}
```

`emit_resource_check` IR 保持 pre-#136 直线形态（call + load + cond_trap）：
- IR-level basic block 不增 → loop opt 启发式不破
- 当 deadline = MAX：indirect call 仍发生（~2.3 ns），但 helper 立即返 0，downstream `icmp 0 >= MAX` trivially false，cond_trap inert
- 当 deadline 设置：helper 走 vDSO，路径无 regression

## Cranelift 0.131 → 0.132 升级

API 兼容（无 breaking change）。试在 0.132 上重 apply b99f2b4 sentinel-skip IR pattern，**仍 +109% regression**。Loop-opt 启发式没修。

升级本身保留：
- baseline cranelift_aot_loop 2.075 ms（与 0.131 同）
- 2227 tests / 0 fail
- 降低未来积压维护

## 最终数字 (Option I + 0.132)

| Row | Pre-#136 | b99f2b4 broken | Option I (现) | 评价 |
|---|---:|---:|---:|:---:|
| `cranelift_aot_loop` | 2.07 ns | 4.33 ns (+109%) | **2.07 ns** | ✅ recover |
| `dispatch_cranelift_step_legacy_i64` | ~60 ns | 14.88 ns | **17.17 ns** | ✅ vDSO 价值 90%+ 保留 |
| `dispatch_cranelift_step` (HashMap) | ~415 ns | ~366 ns | **362 ns** | ✅ 略好 |
| `dispatch_cranelift_step_smallmap` | n/a | 70 ns | **64.5 ns** | ✅ 略好 |

**Net positive across all metrics.**

## Follow-up RFC：Option E（双 variant 编译）

### 设计

```rust
pub struct CraneliftAotEvaluator {
    entry_no_check: extern "C" fn(...),   // IR with deadline_check=false
    entry_with_check: extern "C" fn(...), // IR with deadline_check=true
}

fn run_main(&self, args) {
    if self.sandbox_state.deadline_ns == i64::MAX {
        (self.entry_no_check)(args)   // 0 ns IR-level resource check
    } else {
        (self.entry_with_check)(args) // 包含完整 resource check
    }
}
```

### 收益

- 默认 deadline workload 彻底消除 `now_helper` indirect call (~2.3 ns / dispatch)
- legacy_i64 从 17.17 ns 降回 ~14.88 ns
- HashMap/SmallMap 路径类比受益

### 代价

- **Cranelift compile time × 2**：每 evaluator 多编一份 JIT module（~1-2 ms per W11 case）
- **Memory × 2**：双 JIT module 共存（~10-100 KB code each）
- **ET_DYN cache × 2**：dlopen 路径需缓存两份 binary
- **W11 cold start 退**：单 variant 2.93 ms (× 1.488 vs LuaJIT) → 双 variant ~5 ms (× 2.5+)。**X 1.5 stretch target 会破裂**
- **Break-even 870K invocations / evaluator** — Relon config DSL 主用例（一次性 schema 校验）几乎不可能达到

### 不立项理由

1. 2.3 ns vs cold start ms-scale 是 6 量级差
2. Relon 主用例（config / schema）每 evaluator dispatch < 1K 次
3. 当前 17.17 ns 已远低于 LuaJIT host-boundary 30+ ns
4. 实现复杂度提升（双 variant 同步 / 错配检测 / runtime variant switch 语义）

### 触发条件

- 用户报告 "hot evaluator millions of dispatches needed"
- HFT-类场景需求出现
- Cranelift 大版本（0.133+ 或 1.0）开放 IR-level skip 而不退化时，再 retry b99f2b4 pattern

## 引用

提交链：
- `1b2745b` revert b99f2b4
- `f5857d7` Option I helper fast-return
- `f764ccb` cranelift 0.131 → 0.132 + retry sentinel-skip (rejected)
- `<this commit>` follow-up doc

相关 stage reports：
- `docs/internal/review-improvement-136-dispatch-boundary-2026-05-21.md` — b99f2b4 的 #136 lever 3 原始设计
- `docs/internal/review-improvement-154-dispatch-carryover-2026-05-21.md` — #154 dispatch boundary 续抽
