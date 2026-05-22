# SEC-2 Follow-up Wave 完工 report

**完成日期**：2026-05-22

## 总览

`docs/internal/crate-review-2026-05-22.md` 第二批 "保留 / 仍需后续" 5 项 priority follow-up，全部落地。

**Tests 2316 → 2332 (+16)**。

## 完成情况

| ID | 项 | 修法 | 关键 commit |
|---|---|---|---|
| #174 | IntegrityMode::TrustOnWrite 删除 | Option A 完全 enum variant 移除，object-cache 不再写无 HMAC cache。CHANGELOG Breaking | af9c5fe (merge) |
| #175 | pointer-return string materialise API | JITedTraceFn::invoke_materialised 高阶 API + RawInvokeResult；trace-jit 不再泄漏 raw pointer 到调用方 | fc274c6 (merge) |
| #176 | v1 dict helper / dict_inline 隔离 | Option A cfg(test) 包裹 v1 helpers + dict_inline 整 module；bench fixtures 已用 v2；prod build 验 0 v1 符号 + 10 v2 符号 | 1668e9b (merge) |
| #177 | backend support matrix → harness ratchet | BackendKind + supported_by metadata + ratchet aggregation；silent soft-pass fallback → hard fail；9 negative regression test | aa53349 (merge) |

## 关键收紧

### #174 TrustOnWrite removed
- `IntegrityMode` 现仅 `RequireMatch` (default) / `Strict` 二态
- `hmac_key=None` 不再写 cache（明确 disable，不沉默 trust）
- doc + CHANGELOG breaking 标注

### #175 invoke_materialised
- trace-JIT 返回 string 时 raw 指针 + reclaim 责任不再外泄
- `MaterialisedValue` + `MaterialisedInvokeError` enum
- test-harness StrConcatN trace tests 迁移到新 API

### #176 v1 dict path 隔离
- 双层 `#[cfg(test)]`：runtime helpers (`__relon_trace_dict_lookup` / `_prechecked` / `build_dict_record`) + emitter `dict_inline` 整 module
- runtime/mod.rs + trace-jit/lib.rs + trace-emitter/lib.rs v1 re-export 全删
- bench fixtures (W5/W6/cmp_lua) 已切 v2 (#172 完成)
- prod build: `cargo rustc --release -p relon-trace-jit --emit asm` 验 0 v1 + 10 v2 symbol

### #177 ratchet
- `BackendKind` enum (4 way: TreeWalk/Bytecode/CraneliftAot/TraceJit) 与 `Backend` 区分（trace-JIT 是 cranelift overlay）
- 每个 case `supported_by` metadata：32 4-way + 8 trace-skip + bytecode-out
- `corpus_differential` / `three_way_corpus` / `bytecode_diff` 接 ratchet：claimed support backend 失败/soft-pass = `RatchetViolation`
- 9 negative regression test 防回滑

## 命中 cwd-drift 防御

agent-ab31a4c0392a2042c (#176) 完成后 `git merge` 首报 "Already up to date"，调查发现 cwd 误指向已 unmounted worktree 路径，实际 main 仍 `aa53349`。重置 cwd 后正常 merge 落 `1668e9b`。worktree branch + dir 全清。

memory [[feedback_agent_cwd_drift]] 触发，按 SOP 处理。

## Gate

每 phase 全过：
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`: **2332 / 0 fail**
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`

## 累计累进

| Wave | tests | 累计 |
|---|:---:|:---:|
| 1907 baseline | — | 1907 |
| SEC 主 (7 项) | +409 | 2316 |
| SEC-2 (本 wave 4 项) | +16 | **2332** |

## 剩余 follow-up

`crate-review-2026-05-22.md` 第二批 5 priority 列了 5 项：本 wave 落 4 项 (#174-#177)。第 5 项是 **sigsetjmp/landing-pad recoverable trap**，依旧标 "等 v6-γ trace-recorder deopt 一起做"（需同 setjmp machinery）。

其余 deferred 见 `security-review-wave-completion-2026-05-22.md` "剩余 follow-up"。

## 结论

review-doc 第二批 5 项 priority follow-up 4/5 落地。剩 1 项 sigsetjmp 等条件触发。当前 main 状态：

- HEAD `1668e9b`
- tests 2332 / 0 fail
- crate-review-2026-05-22.md P0+P1+P2+priority 全部解决（剩 sigsetjmp 一项条件触发）
- W12 bytecode × 1.15 ✓
- v1 dict path prod build 完全隔离
- 沉默 fallback 路径全部 hard-fail
