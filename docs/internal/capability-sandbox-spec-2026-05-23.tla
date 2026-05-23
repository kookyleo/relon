------------------- MODULE capability_sandbox_spec_2026_05_23 -------------------
(***************************************************************************)
(* relon Capability / Sandbox / Cache HMAC — minimal TLA+ skeleton.        *)
(*                                                                         *)
(* PURPOSE                                                                  *)
(*   将 docs/internal/formalization-targets-2026-05-23.md F-3 列出的 4 条   *)
(*   invariant 与 Variables / Actions 落到 syntactic 合法的 TLA+ 骨架，    *)
(*   作为 future RFC 触发完整 TLC 形式化的承接面。                          *)
(*                                                                         *)
(* WHAT THIS SPEC PROVES (今日)                                             *)
(*   - Nothing yet. Invariants 当前以 placeholder `TRUE` 留位 (带 TODO)，   *)
(*     使 spec 可被 TLC parser 接受而不做实际状态空间约束。                *)
(*   - State + transitions 的 shape 已经显式：variable name / action name  *)
(*     对齐到代码侧 (`CapabilityBit`, `register_host_fn`, `ensure_key`,    *)
(*     `cache_write`, `cache_read`)。                                       *)
(*                                                                         *)
(* WHAT THIS SPEC DOES NOT PROVE                                            *)
(*   - 不证 INV1 (capability gate)：需要 codebase 中 NativeFnGate /         *)
(*     CapabilityBit 全集枚举落地后再具体化。                              *)
(*   - 不证 INV2 (cache HMAC binding)：需要 #171 cache-hmac 路径的精确     *)
(*     transition modelling。                                                *)
(*   - 不证 INV3 (RequireMatch 模式拒读 key-less blob)：需要 sidecar 流程   *)
(*     enum 拆解。                                                          *)
(*   - 不证 INV4 (schema sidecar 绑定 triple)：需要 entry_shape hash 的     *)
(*     domain abstraction。                                                  *)
(*                                                                         *)
(* FOLLOW-UP TRIGGER                                                        *)
(*   formalization-targets-2026-05-23.md::F-3 — 当 cap RFC 落地 (新 cap     *)
(*   variant / multi-tenant / 第三方 backend) 时，把 INV1-INV4 的 TODO       *)
(*   stub 替换为真实谓词并跑 TLC 状态机 model check。                       *)
(*                                                                         *)
(* TOOLCHAIN                                                                *)
(*   配套 .cfg：capability-sandbox-spec-2026-05-23.cfg                      *)
(*   预期 `tlc capability-sandbox-spec-2026-05-23.tla` 可 parse；当前 CI    *)
(*   不挂 TLA tools，故仅做 syntactic gate。                                *)
(***************************************************************************)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    CapBits,         \* 抽象 capability bit 集合 (e.g. {fs_read, fs_write, net_dial, ...})
    HostFnIds,       \* host fn 索引集合 (u32 域抽象成有限集)
    Schemas,         \* 用户 schema 集合
    Methods,         \* schema 方法名集合
    SourceHashes,    \* source code SHA 集合
    PolicyValues     \* {"Granted", "Denied", "Default"}

VARIABLES
    granted_bits,           \* SUBSET CapBits — 当前已 grant 的位
    gate_policy,            \* [CapBits -> PolicyValues] — 每位的策略
    host_fns,               \* [HostFnIds -> {"None", "Some"}] — 注册位
    native_methods,         \* [Schemas \X Methods -> {"None", "Some"}] — 方法绑定
    hmac_key_provisioned,   \* BOOLEAN — HMAC key 是否已就绪
    cache_state             \* [SourceHashes -> {"Empty", "Written", "Tampered"}]

vars == << granted_bits, gate_policy, host_fns, native_methods,
           hmac_key_provisioned, cache_state >>

----------------------------------------------------------------------------
(* Type invariant — 描述 variable shape，供 TLC 做 type check。            *)

TypeOK ==
    /\ granted_bits \subseteq CapBits
    /\ gate_policy \in [CapBits -> PolicyValues]
    /\ host_fns \in [HostFnIds -> {"None", "Some"}]
    /\ native_methods \in [Schemas \X Methods -> {"None", "Some"}]
    /\ hmac_key_provisioned \in BOOLEAN
    /\ cache_state \in [SourceHashes -> {"Empty", "Written", "Tampered"}]

----------------------------------------------------------------------------
(* Initial state — 全空，所有 policy = Default，无 host fn / method 注册。 *)

Init ==
    /\ granted_bits = {}
    /\ gate_policy = [b \in CapBits |-> "Default"]
    /\ host_fns = [i \in HostFnIds |-> "None"]
    /\ native_methods = [sm \in Schemas \X Methods |-> "None"]
    /\ hmac_key_provisioned = FALSE
    /\ cache_state = [h \in SourceHashes |-> "Empty"]

----------------------------------------------------------------------------
(* Actions — 对应代码侧 ops。                                              *)

Grant(bit) ==
    /\ bit \in CapBits
    /\ gate_policy[bit] # "Denied"
    /\ granted_bits' = granted_bits \cup {bit}
    /\ gate_policy' = [gate_policy EXCEPT ![bit] = "Granted"]
    /\ UNCHANGED << host_fns, native_methods,
                    hmac_key_provisioned, cache_state >>

Deny(bit) ==
    /\ bit \in CapBits
    /\ granted_bits' = granted_bits \ {bit}
    /\ gate_policy' = [gate_policy EXCEPT ![bit] = "Denied"]
    /\ UNCHANGED << host_fns, native_methods,
                    hmac_key_provisioned, cache_state >>

RegisterHostFn(idx) ==
    /\ idx \in HostFnIds
    /\ host_fns[idx] = "None"
    /\ host_fns' = [host_fns EXCEPT ![idx] = "Some"]
    /\ UNCHANGED << granted_bits, gate_policy, native_methods,
                    hmac_key_provisioned, cache_state >>

EnsureKey ==
    /\ hmac_key_provisioned' = TRUE
    /\ UNCHANGED << granted_bits, gate_policy, host_fns,
                    native_methods, cache_state >>

CacheWrite(h) ==
    /\ h \in SourceHashes
    /\ hmac_key_provisioned = TRUE   \* INV2 precondition: 必须有 key
    /\ cache_state' = [cache_state EXCEPT ![h] = "Written"]
    /\ UNCHANGED << granted_bits, gate_policy, host_fns,
                    native_methods, hmac_key_provisioned >>

CacheRead(h) ==
    /\ h \in SourceHashes
    /\ cache_state[h] = "Written"    \* INV3 precondition: 不能读 Empty/Tampered
    /\ hmac_key_provisioned = TRUE
    /\ UNCHANGED vars

Next ==
    \/ \E b \in CapBits : Grant(b)
    \/ \E b \in CapBits : Deny(b)
    \/ \E i \in HostFnIds : RegisterHostFn(i)
    \/ EnsureKey
    \/ \E h \in SourceHashes : CacheWrite(h)
    \/ \E h \in SourceHashes : CacheRead(h)

Spec == Init /\ [][Next]_vars

----------------------------------------------------------------------------
(* Invariants — 对应 formalization-targets-2026-05-23.md F-3 列出的 4 条。 *)
(* 当前以 TRUE 留 placeholder；future RFC trigger 时替换为真实谓词。       *)

\* INV1: 没有 native fn dispatch 在 (granted_bits \supseteq gate.required) 不
\*       满足或 gate_policy[bit] = "Denied" 时发生。
\* TODO: 需 codebase 中 NativeFnGate / CapabilityBit 全集枚举落地，模型化
\*       dispatch action 后才能写为可证形式。当前 spec 不含 dispatch action，
\*       gate 行为隐含于 Grant/Deny 之 enabling condition。
INV1 == TRUE

\* INV2: cache_write 永远不写无 HMAC (issue #171 修过的)。
\* 在当前 CacheWrite action 的 enabling condition 中已 enforce
\* hmac_key_provisioned = TRUE，但作为 invariant 需要 history variable 来
\* assert "any Written state 之 transition path 上必经 hmac_key_provisioned"。
\* TODO: 引入 history variable / temporal property 后改写为真实表达。
INV2 == TRUE

\* INV3: RequireMatch mode 永不读 key-less blob。
\* TODO: 需 sidecar enum (Off / Optional / RequireMatch) 三态建模。当前
\*       cache_state 抽象未拆 sidecar 维度。
INV3 == TRUE

\* INV4: schema sidecar HMAC 绑定 (source_hash, object_sha256, entry_shape)
\*       三元组 — 篡改任一立即 invalidate (\rightarrow Tampered).
\* TODO: 拆 cache_state 为 record，加入 object_sha256 / entry_shape 维度后
\*       才能 model "tampering 任一 leg \Rightarrow Tampered" 的迁移。
INV4 == TRUE

\* 复合 invariant —— TLC 单次跑可一并 check。
Invariants == TypeOK /\ INV1 /\ INV2 /\ INV3 /\ INV4

============================================================================
