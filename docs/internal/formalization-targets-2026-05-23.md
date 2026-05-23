# Targeted Formalization Backlog

**编制日期**：2026-05-23  
**来源**：用户问 "需不需要形式化验证" → 答 "全栈不必，局部边界值得" → 这份 list 是局部边界的清单。

## 立场

- **不做**：全 IR / 全 evaluator 的 Coq/Lean 证明。配置语言的 spec 实质就是 "tree-walk 跑出什么"，无独立 paper-level 语义；写 spec 会变成 "Rust 翻译成 Coq 再 hash-compare"，重做不增信息。
- **不做**：parser 形式化。rowan CST 已 lossless round-trip + 200+ corpus 测试覆盖。
- **做**：sandbox / capability / cache HMAC 模型 + JIT unsafe helpers 边界算术 + 跨 backend 差分 oracle。详见下表。

## 已有等价物

| 维度 | 当前做法 | 强度 |
|---|---|---|
| Backend correctness | corpus_differential / three_way / four_way + ratchet (silent fallback → hard fail) | 强 |
| Wire format pin | golden CST round-trip + schema layout asserts | 中 |
| `unsafe` 隔离 | `#[forbid(unsafe_code)]` on relon, relon-evaluator, relon-trace-recorder, relon-wasm, relon-cli, relon-lsp, relon-fmt | 强 |
| Concurrency | multi_thread_run_main + hot_counter no_race tests | 中 |
| Cache integrity | HMAC sidecar binding + object_cache_integration tests | 中 |
| Property tests | `proptest` workspace dep | 部分覆盖 |

## 增量 TODO

| ID | 项 | Cost | ROI | 触发 |
|---|---|---|:---:|---|
| F-1 | Miri CI sweep on unsafe modules | 1d | 高 | 任何新 unsafe block 落地前 |
| F-2 | Kani bounded model check JIT str/dict layout arithmetic | 2-3d | 中-高 | trace-jit 加新 runtime helper / dict layout 改 |
| F-3 | Capability/sandbox TLA+ spec | 1-2w | 中 | 加 NativeFnGate variant / cache 拓展 / multi-tenant 出 RFC |
| F-4 | Trace JIT deopt invariant prop-test | 3-5d | 中-高 | 调 deopt site / guard kind 加 variant |
| F-5 | wire-format smoke gate for ConstString migration | 1d | 中 | 想推 ConstString 4B→12B wire flip 之前 |

### F-1 Miri CI sweep

**位置**：CI 配置 + 标 `#[cfg_attr(miri, ignore)]` for tests that hit FFI / mmap (cranelift JIT actual execution can't run under Miri — but the host-side helper logic + sandbox state ops can).

**覆盖**：
- `relon-codegen-native::sandbox` (TrapKind / capabilities snapshot / arena ops)
- `relon-trace-jit::runtime::{dict_list, str_ops, ic_lookup, deopt}` (offset arithmetic, slice::from_raw_parts callers)
- `relon-trace-recorder::recorder::SsaAllocator` (overflow path)
- `relon-bytecode::arena` (handle slot math)

**预期 finding**：可能 0 (现有代码已严谨)；任何 fire 都是真问题，因为 Miri ≠ 模糊器。

### F-2 Kani BMC

**目标 helpers**:
```
__relon_trace_dict_lookup_v2 (entry_count * 24 ≤ record_len)
__relon_trace_dict_lookup_prechecked_v2
__relon_str_concat_n_alloc (n × ptr_size 不溢)
__relon_str_substring (start + len ≤ payload_len, char boundary)
build_dict_record_v2 (header + entries 不溢 u32)
```

**Harness 模板**：每个 helper 一个 `#[kani::proof]` 函数，构造任意符号输入 + 调用 + assert 没 panic / OOB / overflow。

**Cost**：kani setup (~半天) + per-helper 10-30 行 harness × 5 = 2-3 天。

**风险**：kani 不支持 raw pointer 全模型。需用 `kani::any_slice` 或类似 trick。

### F-3 Capability/sandbox TLA+ spec

**Variables**:
```
granted_bits: SUBSET CapabilityBit
gate_policy: [CapabilityBit -> {Granted, Denied, Default}]
host_fns: [u32 -> Option<RelonFunction>]
native_methods: [(Schema, Method) -> Option<NativeFn>]
hmac_key_provisioned: BOOLEAN
cache_state: [path -> {Empty, Written(hmac_tag), Tampered}]
```

**Actions**:
- `grant(bit)` / `deny(bit)`
- `register_host_fn(idx, gate)`
- `ensure_key()` → flips `hmac_key_provisioned`
- `cache_write(source_hash, object_bytes)` (requires `hmac_key_provisioned`)
- `cache_read(source_hash)` (requires HMAC verify)

**Invariants**:
- INV1：no native fn call dispatches without (granted_bits ⊇ gate.required) ∧ gate_policy[bit] ≠ Denied
- INV2：`cache_write` 永远不写无 HMAC（`#171` 修过的）
- INV3：`RequireMatch` mode never reads key-less blob
- INV4：schema sidecar HMAC binds `(source_hash, object_sha256, entry_shape)` triple — tampering any leg invalidates

**Win**：spec doc 让 future cap 加 variant / cache 拓展时机械化检查"是否破坏 INV1-4"。代码里现有 implementation tests 在 INV 上 implicit；spec 让 invariant 显式。

### F-4 Trace JIT deopt invariant

**Property**：
```
∀ trace T, guard_pc g ∈ T.guards:
  let snap = deopt_snapshot(T, g) in
  let eager_state = tree_walker_run_until(T.ir, g.external_pc) in
  snap.ssa_stack ≡ eager_state.value_stack[..g.ssa_stack_depth]
  ∧ snap.pending_writes ≡ eager_state.recoverable_writes
  ∧ snap.external_pc == g.external_pc
```

**Implementation**：proptest generator for trace bodies (Op subset the recorder supports) + tree-walker oracle.

**Cost**：3-5 天 (生成器 + shrinker + invariant 断言 + 修 CI 发现的 deopt 漂移)。

**Win**：catches deopt regressions that current ratchet only flags as outcome diff (i.e. catches "answer correct but state corrupted" — silent UB precursor).

### F-5 wire-format smoke gate

**Why**：`docs/internal/review-improvement-169-conststring-wire-full-2026-05-22.md` Plan C 文档化了 ConstString 4B→12B wire flip 的 5 个 blocker，第一个就是 "缺 wire_format_smoke test"。当前 corpus_differential 检查答案不查字节，所以 misclassification 可能携错字节但 MatchOk。

**Implementation**：for each backend pair (tree_walk × bytecode / cranelift / trace_jit)，对 fixed corpus 比对 emit output 的精确字节（schema header + payload bytes）。差一字节就 ratchet fail。

**Cost**：1 天写 harness，之后维护成本 ≈ 0。

**Win**：解锁后续 wire 调整（ConstString 升 12B 头 / dict_v2 entry stride 调）的安全空间。

## 触发策略

- **F-1 / F-5**：低 cost，下个普通 session 顺手就做。
- **F-2 / F-4**：需要 dedicated 1-3 天，等 trace-jit 或 dict layout 有新 RFC / 改动时一起做。
- **F-3**：等"多租户" / 新 cap 变体 / 第三方 backend 等 RFC-级触发条件，单独 wave。

不要为做而做。当前 sandbox + cache + dispatch correctness 由差分 + ratchet + golden corpus 覆盖到 release-ready 水平；这份清单是"想再加一层" 的工具箱，不是必修。
