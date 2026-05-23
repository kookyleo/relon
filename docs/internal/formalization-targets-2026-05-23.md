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
| F-1 | Miri CI sweep on unsafe modules | **DONE** | 高 | `.github/workflows/ci.yml::miri` job (2026-05-23) |
| F-2 | Kani bounded model check JIT str/dict layout arithmetic | 2-3d | 中-高 | trace-jit 加新 runtime helper / dict layout 改 |
| F-3 | Capability/sandbox TLA+ spec | 1-2w | 中 | 加 NativeFnGate variant / cache 拓展 / multi-tenant 出 RFC |
| F-4 | Trace JIT deopt invariant prop-test | 3-5d | 中-高 | 调 deopt site / guard kind 加 variant |
| F-5 | wire-format smoke gate for ConstString migration | **DONE** | 中 | byte-pin tests + cross-link doc landed 2026-05-23 |

### F-1 Miri CI sweep — DONE 2026-05-23

**位置**：`.github/workflows/ci.yml::miri` job runs `cargo +nightly miri test` on:
- `relon-eval-api` (SmolStr SSO + `from_utf8_unchecked` path)
- `relon-trace-abi` (no unsafe but counts as bottom-of-stack sanity)
- `relon-trace-jit --lib` (dict_list / str_ops / ic_lookup / deopt offset arithmetic + slice::from_raw_parts callers)
- `relon-trace-recorder`
- `relon-bytecode --lib` (arena handle slot math)

**NOT covered**：`relon-codegen-native` pulls cranelift / wasmtime whose generated nightly code uses unstable `vec_into_raw_parts` etc.; the pinned miri rustc rejects it. `relon-wasm` (browser target). Both keep stable-job coverage.

**已发现 + 修过**：
- `relon-trace-jit::runtime::call_table::resolve_is_fast_with_thousand_entries` 是 perf-threshold test，miri 慢解释下不可能过。`#[cfg_attr(miri, ignore = "miri interprets, ns/lookup target meaningless")]` 标了。

**UB findings**：0。当前 unsafe surface 在 miri 抽象语义下干净。

**MIRIFLAGS**：`-Zmiri-disable-isolation` (允许 mmap-like syscalls; miri 默认 isolation 对 SmolStr Arc<str> + 一些 Vec ops 误报)。

**预算**：每 PR 在 CI 上加 ~5-8 分钟 (miri 解释速度比 native 慢 100-1000x)。timeout-minutes: 30。

### F-2 Kani BMC — DONE 2026-05-23

**位置**：`crates/relon-trace-jit/src/runtime/proofs.rs` (cfg(kani) 门) + CI `kani` job 用 `model-checking/kani-github-action@v1`。

**4 proofs verified**:
1. `dict_v2_entry_table_bounds_valid` — entries-end gate (`12 + entry_count * 24 ≤ record_len`) 通过 → 任意 `i ∈ [0, entry_count)` 的 entry 末字节仍在 record 内。
2. `dict_v2_stored_payload_bounds_imply_in_record` — post-hash payload bounds (`stored_off ≥ entries_end ∧ stored_end ≤ record_len`) 通过 → `from_raw_parts(stored, stored_len)` 切片在 record 内。
3. `str_concat_n_alloc_cursor_stays_in_payload` — 每 operand 通过 `cursor + r.len ≤ total_len` → 最终 cursor ≤ total_len，写入都在 allocation 内。`#[kani::unwind(5)]` bound MAX_INLINE_STR_CONCAT_N=4 防 SAT blowup。
4. `str_substring_clamp_keeps_inside_payload` — start/len/payload_len clamping → `start' ≤ end' ≤ payload_len`。

**关键 lessons**:
- 不证 unsafe extern "C" 本身（kani 无法 model 任意 raw pointer）；改证 **layout arithmetic**（saturating ops + bounds 比较）的纯算术性质。这覆盖 helper 安全性的最关键不变量。
- 符号 loop bound (`for _ in 0..n` where `n: usize = kani::any()`) 会让 CBMC SAT solver 卡死（实测 47+ 分钟 timeout）。fix：`#[kani::unwind(5)]` + `kani::assume(n <= 4)` 双约束。

**未做** (留 follow-up):
- `build_dict_record_v2` 的 header + entries 不溢 u32（doc 列了 5 项但只 prove 4 项，build 那条算术覆盖在 dict_v2_entry_table_bounds_valid 里反向蕴含）。
- `decode_pointer_header` / consumer-side layouts。

**Cost 实际**：~1.5h (writing + 1 次 SAT-stuck debug)，远低于 doc 预估 2-3d。

**Win**：integer-overflow / OOB-read 类的 layout-arithmetic bug 在 checked 配置下被 SMT solver 数学证明不可达。

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

### F-5 wire-format smoke gate — DONE 2026-05-23

**位置**：
- `relon-eval-api::buffer::write_string_wire_format_smoke_gate` — buffer-protocol producer 侧 byte-pin
- `relon-codegen-native::codegen::const_pool::opvisitor_emits_const_string_record_in_declaration_order` — cranelift const-pool producer 侧 byte-pin (扩了 doc 包含 migration trigger 描述)

两 test 互相 cross-link，doc 明确指向 `review-improvement-169-conststring-wire-full-2026-05-22.md` 的 5 个 blocker。任何修改任一 producer 的字节 layout 都会立即 fire 这两个 test，强制 migrant 在同一 commit 内更新所有 5 个 consumer (#164 silent-corruption regression 的根因被 ratchet 锁住)。

**未做** (留 follow-up)：
- consumer-side 配对 pin 在 `decode_pointer_header` / `emit_read_string_len` / `emit_tail_record_from_absolute` 各加一个 byte-shape test
- bytecode VM `StrConst` arena 写法不是 wire format (走 arena handle)，无需 pin

**Cost**：实际 ~1h (两 test + 三段 cross-link doc)。
**Win**：ConstString migration 现在有 byte-level guard rail，#164 silent-corruption 类的 regression 不可能再 silent 落地。

## 触发策略

- **F-1 / F-5**：低 cost，下个普通 session 顺手就做。
- **F-2 / F-4**：需要 dedicated 1-3 天，等 trace-jit 或 dict layout 有新 RFC / 改动时一起做。
- **F-3**：等"多租户" / 新 cap 变体 / 第三方 backend 等 RFC-级触发条件，单独 wave。

不要为做而做。当前 sandbox + cache + dispatch correctness 由差分 + ratchet + golden corpus 覆盖到 release-ready 水平；这份清单是"想再加一层" 的工具箱，不是必修。
