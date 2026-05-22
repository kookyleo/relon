# Relon 全项目逐 crate review

日期：2026-05-22  
范围：workspace 内 20 个 crate；源码静态 review、风险 API 搜索、关键安全边界抽样核对、全量 Rust gate。  

验证命令：

- `cargo fmt --all -- --check`：通过
- `cargo test --workspace`：通过
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过
- `git diff --check`：通过

## 结论

项目主线质量不错：parser / analyzer / evaluator / bytecode / facade / CLI / LSP / wasm 基本保持 safe Rust，测试密度高；native AOT、object cache、trace runtime 这些高风险区已经补上了几处近期 review 问题，包括 per-call `SandboxState`、object/schema HMAC、atomic hot counter、evaluator import pin 校验。

本轮已修复 / 改进：

- `SmolStr` 从 public enum 改成 public struct + private repr，safe inline builder 增加 UTF-8 校验，关闭 safe 代码制造非法 `&str` 的路径。
- remote import cache 的 `<digest>.body` / `<digest>.meta` 读取会校验 `body_sha256`，不匹配时删除缓存并返回 cache miss。
- `IntegrityMode::TrustOnWrite` 标记为 deprecated，内部 legacy 测试显式 `allow(deprecated)`，对生产 caller 给出迁移信号。
- `trap_handler` 不再安装 Rust process-wide SIGSEGV/SIGFPE/SIGILL handler；历史 installer / TLS slot API 变为 no-op，signal 只保留到 `TrapKind` 的诊断映射。
- trace string arena 增加 ownership-aware `invoke_with_string_reclaim`；标准 resume 路径在 guard/abort/fallback 和非指针 success 返回上做 scoped reclaim。
- trace dict lookup active ABI 切到 v2：IR/recorder/TraceBuffer 携带 `record_len_hint`，emitter/installer 注册 `__relon_trace_dict_lookup_v2` / `__relon_trace_dict_lookup_prechecked_v2`，缺失长度时安全 deopt。
- README / SECURITY 同步到当前 crate 架构和 unsafe island 威胁模型。

仍需后续产品化的项：`IntegrityMode::TrustOnWrite` 在 semver 允许时移除；trace string 返回值为 arena 指针的 success 路径仍要求 caller 使用 scoped closure API 复制/消费结果；硬件 fault 恢复若要从 fail-fast 升级为 typed trap，需要 C-side `sigsetjmp`/landing-pad 方案。

## 主要发现

### P0（已修复）：`SmolStr` 的 safe 公共构造面可制造非 UTF-8，破坏 `as_str()` 的 unsafe 前提

原问题：`SmolStr` 是 public enum，`Inline { len, data }` 变体和字段也对下游可见；`as_str()` 对 inline payload 使用 `std::str::from_utf8_unchecked`，但 safe 下游可以直接构造非法 UTF-8，`try_build_inline` 也只依赖文档约束。

修复：`SmolStr` 已改为 public struct，内部 `SmolStrRepr` 私有；`try_build_inline` 在构造前调用 `std::str::from_utf8(&data[..out_len])`，非法字节返回 `None`。新增 `try_build_inline_rejects_invalid_utf8`，并保留 24-byte size guard。

### P1（已修复）：process-wide signal handler 使用 Rust TLS，不满足 async-signal-safe 前提

原问题：早期 `trap_handler` 安装 process-wide SIGSEGV/SIGFPE/SIGILL handler，并在 handler body 写 Rust TLS；这不满足 POSIX async-signal-safe 前提，也可能在非 JIT 线程触发。

修复：`install_global_signal_handler()` / signal slot reset/read 保留兼容 API 但全部变成 no-op；typed traps 只来自结构化 `SandboxState::trap_code`；硬件 faults 在没有真实恢复 trampoline 前保持平台默认 fail-fast。`signal-hook` / `signal-hook-registry` 依赖已移除，signal 常量映射改用 `libc`。

### P1（已改善）：trace string arena 已接入 scoped reclaim，指针返回仍需显式 materialise

原问题：`relon-trace-jit` 提供 `reclaim_trace_strings()`，但 codegen-native 的标准 trace invoke 包装没有统一调用；长生命周期 host 直接使用 trace install path 时会累积 string shim 分配。

修复：新增 `JITedTraceFn::invoke_with_string_reclaim`，让 caller 在闭包内读取 / materialise result 和 deopt snapshot，闭包退出后 RAII reclaim arena。`TraceJitState::invoke_with_resume` 在 guard failed / aborted fallback 路径使用 scoped reclaim；success 路径会在返回 observed type 明确是非指针时自动 reclaim。

保留 caveat：不能在所有 success 返回上无条件 reclaim，因为返回值可能本身是 arena 分配的 string 指针。指针返回的 caller 必须使用 scoped closure API 在 reclaim 前复制/消费结果；`invoke_raw` 仍是低层 unsafe API，caller 自行负责 reclaim。

### P2（已修复）：trace dict lookup active path 已切到碰撞安全 v2 helper

原问题：v2 helper 已经能做 `record_len` bounds check 和 key payload byte-compare，但 active emitter / installer 仍注册 v1 hash-only 符号。

修复：`HostHookId::DictLookup` / `DictLookupPrechecked` 现在解析到 `_v2` 符号；Cranelift helper signature 增加 `record_len`；`TraceBuffer` 增加 `dict_record_len_hints`；`Op::DictGetByStringKey` / recorder / lowering / bench fixture 向 trace 传递 record envelope 长度。缺失 `record_len_hint` 时 emitter 传 `record_len = 0`，v2 helper 在读取 dict body 前 deopt，不回退到 v1。

保留状态：v1 helpers 和 `dict_inline` v1 inline 实验代码仍留在 `relon-trace-jit` / `relon-trace-emitter` 中作为 legacy tests / reference-only 路径；active install path 不再使用它们。

### P2（已修复）：remote import cache 存了 `body_sha256`，但 cache hit 和 304 路径没有校验

原问题：remote cache metadata 记录了 body SHA-256，但 `load()` 读出 `<digest>.body` 和 `<digest>.meta` 后直接返回 `CachedEntry`，fresh cache hit 和 304 revalidation 都可能继续使用本地损坏或被投放的 body。

修复：`RemoteImportCache::load` 现在比较 `body_digest(body)` 和 `meta.body_sha256`；不匹配时删除 body/meta 两个文件并返回 `None`，让 resolver 走网络 re-fetch。新增 `cache_body_digest_mismatch_is_cache_miss` 覆盖篡改 body 的路径。

### P2（已改善）：`IntegrityMode::TrustOnWrite` 仍是 public footgun

原问题：object cache integration 已经改成 HMAC key 不可用时拒绝读写，并用 `IntegrityMode::HmacRequired` 加载；但 `relon-object-cache` 仍公开 `IntegrityMode::TrustOnWrite`，外部 caller 可以选择“跳过对象体 SHA-256 且不要求 HMAC”的降级模式。

修复状态：`TrustOnWrite` variant 已加 `#[deprecated]`，note 明确要求生产 caller 使用 `Strict` 或 `HmacRequired`。crate 内 legacy 测试保留覆盖，并显式 `#![allow(deprecated)]`，避免把测试 fixture 伪装成推荐 API。

### P3（已修复）：安全文档和 README 已经落后于当前架构

原问题：`SECURITY.md` 仍承诺 workspace 全部 `forbid(unsafe_code)`，但当前 native AOT、object cache loader、trace ABI/runtime、`SmolStr`、IR SIMD/shape hash 等都有受控 unsafe 区域。`README.md` 的 Project Structure 也仍写 parser built with `winnow`，并列出已退休的 wasm-AOT backend。

修复：`SECURITY.md` 已改成“safe core + audited unsafe islands”的模型；`README.md` 已同步 rowan parser、bytecode、native、object-cache/link、trace crates、test harness。

## 已核对的近期修复

- native evaluator 并发状态：`CraneliftAotEvaluator` 现在持有 `SandboxShared` 模板，每次 dispatch 新建 per-call `SandboxState`（`crates/relon-codegen-native/src/evaluator.rs:128`、`:147`；`crates/relon-codegen-native/src/sandbox.rs:307`、`:322`）。
- object/schema cache 认证：HMAC key 不可用时拒绝读写；load 使用 `HmacRequired`；schema sidecar v2 HMAC 绑定 source hash、object hash、entry shape/arity（`crates/relon-codegen-native/src/object_cache_integration.rs:51`、`:64`、`:456`、`:486`；`crates/relon-codegen-native/src/schema_cache.rs:36`、`:40`、`:197`、`:220`）。
- trace hot counter：全局 counter 已改为 `AtomicU32`，JIT 侧走 atomic RMW（`crates/relon-codegen-native/src/trace_install.rs:26`、`:36`、`:135`、`:143`）。
- evaluator import integrity：tree-walk evaluator 已把 `sha256:"..."` pin 传入 `load_module` 并验证（`crates/relon-evaluator/src/eval.rs:1046`、`:1057`、`:1283`、`:1284`），并有 `import_pin_tests` 覆盖 analyzer-bypass 场景。

## 逐 crate review

### `relon`

职责：公共 facade、`EvaluatorBuilder`、`AutoEvaluator`、JSON 边界、宿主入口。

状态：crate root `forbid(unsafe_code)`；backend 选择和 tree-walk fallback 路径测试充足。默认 feature 仍启用 `cranelift-aot`，所以 native / cache / trace 风险会经 facade 传播给普通 host。

建议：在 native/trace 风险修完前，facade 文档应明确 `Auto` 的 cache、threading、unsafe island 边界；对 untrusted host 建议默认 tree-walk / bytecode。

### `relon-parser`

职责：lexer、rowan CST、legacy AST lowering、recovering parser、source range。

状态：`forbid(unsafe_code)`；CST round-trip、broken fixture、proptest 覆盖较好。parser 仍使用递归下降结构，深嵌套输入的 stack DoS 是已在 `SECURITY.md` 声明的限制。

建议：继续给 fast path / slow path 加等价测试，尤其新语法进入 facade 的 trivial fast path 时。

### `relon-analyzer`

职责：workspace build、diagnostics、strict mode、schema/typecheck、import hash pin。

状态：`forbid(unsafe_code)`；workspace import pin、cycle、cross-module、strict matrix 覆盖强。安全边界比 evaluator 更完整，但 evaluator 已补 analyzer-bypass pin 校验。

建议：继续把 analyzer 输出做成更强类型边界，减少后端直接消费未验证 AST 的空间。

### `relon-eval-api`

职责：`Value`、`Scope`、`Context`、schema/layout/buffer、`Evaluator` trait。

状态：大部分 `deny(unsafe_code)`；`SmolStr` 已封装 public representation，并让 safe inline builder 校验 UTF-8，原 P0 soundness 路径已关闭。

建议：考虑把公共 API 内 `Mutex::lock().unwrap()` 的 poisoning 策略文档化，长期服务型 host 更适合 typed error。

### `relon-evaluator`

职责：tree-walk reference evaluator、stdlib、module resolver、sandbox capability gate。

状态：`forbid(unsafe_code)`；capability gate、stdlib purity、import pin 回归测试较好。remote import cache 现在会在读取时校验 `body_sha256`，损坏/篡改的 body 会被 evict 并触发 re-fetch。

建议：untrusted remote imports 仍建议默认配合 `--require-hash` / analyzer require_hash，因为本地 cache 校验不能替代远端内容 pin。

### `relon-ir`

职责：AST/analyzer 到 IR lowering、stdlib IR、Unicode / shape hash。

状态：crate root `deny(unsafe_code)`，unsafe 限在 SIMD/shape hash 等小岛。IR 是多后端共享契约，新增 op 时容易出现 recorder / emitter / bytecode / native 不同步。

建议：保持 visitor/exhaustiveness 测试；Unicode 生成数据继续保留版本/freshness gate。

### `relon-bytecode`

职责：stack VM、bytecode compiler/evaluator、trace/deopt handoff。

状态：`forbid(unsafe_code)`；sandbox、partial resume、hot counter、differential harness 覆盖强。当前 fallback/unsupported 策略适合渐进式扩大覆盖。

建议：对已经宣称支持的 tier，把 fallback 从 soft-pass 逐步 ratchet 成 failure。

### `relon-codegen-native`

职责：Cranelift native AOT/JIT、sandbox state、object/schema cache、trace install glue。

状态：高风险但近期修复明显：per-call sandbox、mandatory HMAC、schema sidecar HMAC、atomic hot counter、no-op signal handler policy、trace string scoped reclaim、dict lookup v2 active ABI 都已落地。

建议：扩大 dlopen/trace 默认启用面前继续补并发/arena 所有权测试；每个 `unsafe impl Send/Sync` 保持并发模型测试。

### `relon-object-cache`

职责：object cache blob、metadata、HMAC key、memfd/dlopen loader。

状态：HMAC-required 模式和 loader smoke tests 清楚。`TrustOnWrite` 仍为兼容保留，但已经 deprecated，外部 caller 会收到迁移警告。

建议：下一步可以在 semver 允许时移除或进一步限制 `TrustOnWrite`；API 层继续表达“未认证 object bytes 不可执行”。

### `relon-object-link`

职责：ET_REL -> ET_DYN 链接，默认 subprocess linker。

状态：代码边界清楚，Linux x86_64 假设明确。`RELON_LD` / `$PATH` linker 是 operator trust boundary，适合 native cache 场景。

建议：文档里继续强调 host triple、linker、`RELON_LD` 信任边界；非 Linux 平台保持 clean fallback。

### `relon-trace-abi`

职责：trace JIT 共享 ABI 类型、hook table、deopt 状态。

状态：layout / discriminant / serde roundtrip 测试好。`HostHookTable` 的 Send/Sync 合理依赖 host 安装的函数指针本身可跨线程调用。

建议：新增字段时必须同步 offset/size tests；文档说明 ABI 是同一 Rust build 内部 ABI，不是稳定 C ABI。

### `relon-trace-emitter`

职责：Trace IR -> Cranelift IR，guard、inline dict/string/list lowering。

状态：IR verifier、inline emit sync lint、helper 声明测试较强。active dict lookup 已走 v2 helper；缺失 record length 会安全 deopt。

建议：继续补端到端 hash collision、record_len truncation、prechecked path 的跨 crate smoke；legacy v1 `dict_inline` 若长期不恢复，应在后续删除或移到 fixture-only 模块。

### `relon-trace-recorder`

职责：从 IR/op stream 记录 trace、类型观察、effect 分类、unsupported abort。

状态：safe Rust；abort/guard/type observation 覆盖充分。设计上遇到 unsupported 会 abort/fallback，适合当前阶段。

建议：把支持矩阵和 harness 趋势绑定，避免某类 op 长期 silent abort。

### `relon-trace-jit`

职责：trace IR、optimizer、runtime helpers、string/dict/list/deopt/call-table helper。

状态：runtime helper 是 unsafe 密集区。string arena 已有 scoped reclaim 调用面；dict v2 helper 已成为默认 emitter/installer 路径，v1 helper 仅 legacy 保留。

建议：继续收窄 v1 dict helper 暴露面；为 pointer-return string success 路径提供更高层 materialise API，减少 caller 误用 `invoke_raw` 的空间。

### `relon-test-harness`

职责：多后端 differential corpus、three/four-way comparison。

状态：项目质量关键资产；本轮全量测试通过，说明主线后端没有已知 corpus 分叉。

建议：更细分 fallback / unsupported / trap 分类；当某 backend 宣称支持某 tier 时，harness 应把 fallback 变成失败。

### `relon-fmt`

职责：formatter、syntax checker CLI。

状态：`forbid(unsafe_code)`；idempotence、preset、syntax 组合覆盖不错。

建议：继续把新语法加入 golden corpus，尤其 import integrity、generic schema、schema methods、match/tuple 组合。

### `relon-cli`

职责：命令行入口、backend flag、workspace load、sandbox/trust flag、输出。

状态：`forbid(unsafe_code)`；默认 sandbox 姿态较好；native auto/cache 通过 facade 进入 CLI。

建议：`--trust` / remote import / cache 行为要在 CLI help 和 docs 中保持同步；如果默认走 disk cache，需让安全文档解释 HMAC key 和失败降级。

### `relon-lsp`

职责：Language Server、diagnostics、hover/completion/definition/references/rename 等。

状态：`forbid(unsafe_code)`；单元测试覆盖主要 feature。风险主要是多文件编辑顺序和 workspace state 清理。

建议：补更多 edit-sequence 测试：新增、删除、rename、hash pin mismatch 修复后的 diagnostics 清理。

### `relon-wasm`

职责：browser wasm bindings、in-memory 多文件 evaluate/format/diagnostics。

状态：`forbid(unsafe_code)`；native unit tests 覆盖 bindings 逻辑，且不会引入 native AOT/cache 风险。

建议：发布 gate 补 `wasm-pack test` 或 headless browser 等价测试，验证 JS binding、panic/diagnostic 序列化和 playground 真实运行环境。

### `relon-bench`

职责：内部 benchmark harness、methodology validators、LuaJIT/native/trace/bytecode 对比。

状态：library 层 `forbid(unsafe_code)`，criterion benches 内有必要的 unsafe FFI/JIT 调用。适合内部性能测量，不应作为 production pattern 复制。

建议：bench-only unsafe wrapper 保持醒目标注；性能报告继续写清 sample size、tail percentile、fixture 假设。

## 优先级建议

1. 在 semver 允许时移除或进一步限制 `IntegrityMode::TrustOnWrite`。
2. 为 pointer-return string success 路径提供高层 materialise API，避免 caller 直接管理 arena 指针生命周期。
3. 删除或隔离 legacy v1 dict helper / `dict_inline`，除非短期内要把它们升级为 v2 inline lowering。
4. 继续把 backend 支持矩阵接到 differential harness，把已经宣称支持的 tier 从 fallback ratchet 成 failure。
5. 若要把硬件 fault 从 fail-fast 升级为 typed trap，单独设计 C-side `sigsetjmp`/landing-pad 恢复机制。

## Gate 结果

修复后复跑：

- `cargo fmt --all -- --check`：通过
- `cargo test --workspace`：通过
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过
- `git diff --check`：通过
