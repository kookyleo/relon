# Review 改进最终完工 report (Wave D)

**完成日期**：2026-05-21

## 总览

继 Wave 1 + A + B+C/Phase4 后，**Wave D 推进 11 项**（#146-#156）覆盖 bytecode M2-B phase 4 收口 + M2-C close LuaJIT gap + M3 closure scaffold + String performance Tier 1-2 全套 + dispatch 边界 carry-over + codegen OpVisitor 完整接入 + glob_match 4 backend wire-up。

**Tests 2161 → 2227 (+66)**。11 项全完。**bytecode W12 ratio vs LuaJIT 4.15× → 1.84×**。**W5 dict_str_key ratio × 1.473 → × 1.151**。

## 完成清单

| ID | 项 | LoC | tests | 关键数字 |
|---|---|---:|:---:|:---:|
| #146 | phase 4c-cont (dispatcher switch + deopt resume) | +1362/-3 | +13 | trace bypass dispatch loop |
| #147 | M2-C (close LuaJIT gap) | +668/-38 | +0 | **W12 448 → 181 ns (-60%), 4.15× → 1.84×** |
| #148 | M3 phase 1 (closure VM surface) | +1026/-5 | +10 | scaffold landed, 0 workload unlocked |
| #149 | String Tier 1a (hash header) | +214/-186 | +0 | **W5 168.52 → 131.08 µs (-22%), × 1.473 → × 1.151** |
| #150 | String Tier 1b (SSO 24-byte) | +1004/-168 | +9 | sso/concat -82% / W3 -16% |
| #151 | String Tier 2a (compile-time intern) | +497/-41 | +7 | dedup + latent bug fix |
| #152 | String Tier 2b (concat tree single-alloc) | +485/-1 | +8 | **inline -69% / heap -59%** |
| #153 | String Tier 2c (ASCII flag fast-path) | +657/-28 | +14 | **upper/lower -86~87%** |
| #154 | #136 carry-over (4 lever) | +581/-39 | +1 | typed_i64 -10.96%, SmallMap NEW 70 ns |
| #155 | glob_match bytecode + trace-JIT wire | +821/-7 | +11 | 4 backend full coverage |
| #156 | codegen Codegen impl OpVisitor | +751/-393 | +0 | 4 backend unified dispatch |

总：+10,000+/-900+ LoC，+66 tests。

## 重大成果

### Bytecode VM 闭环

**phase 4c-cont (#146)** 让 bytecode 完整 dispatcher switch：检测 installed trace → bypass dispatch loop → invoke trace fn 直接；deopt 回 bytecode resume_from_deopt。形成完整闭环：bytecode dispatch → hot counter → trace recording → JIT install → 下次直接 trace fn → guard miss → bytecode resume。

**M2-C (#147)** close LuaJIT gap：W12 bytecode **448.59 → 181.39 ns (-60%)**，ratio vs LuaJIT **4.15× → 1.84×**。Lever 1 typed-i64 fast path 主拳 (-260 ns)，Lever 2 inline cache for stdlib + Lever 5 cache main_schema 补刀。Lever 3 per-op spec + Lever 4 threaded dispatch (need unstable Rust become/computed goto) 诚实留 blueprint。

**M3 phase 1 (#148)** closure VM surface scaffold (ClosureArena + 3 BcOp + 8 test)。**0 workload unlocked** —— iter stdlib (range/map/reduce/sum) body 用 buffer-protocol ops (LoadI64AtAbsolute / BitAnd / alignment math) bytecode VM 没有，需 IR desugar 或 buffer-protocol op family 后续 phase。

### String Performance 全套

**#149 hash header**：W5 **× 1.473 → × 1.151**，dict-key record [len:u32][hash:u64][payload] 12-byte header，trace-emitter dict_inline 用 single load.u64 替 hash compute loop。

**#150 SSO (SmolStr)**：24-byte slot 22-byte inline cap，niche-discriminated via Arc<str>。177-call-site migration。sso/concat_short **2.82 µs → 500 ns (-82%, 5.6× faster)**。W3 tree_walk -16% (受限于 22 B cap heap-fallback tail dominates)。

**#151 compile-time intern**：const-pool dedup latent bug fix (idx 0 reuse 不同 bytes) + intern.rs HashMap dedup。W5/W6 bench flat (-0.23% / -0.81% null)：hand-built recorder IR 不走 Op::ConstString，#149 已 dominate hash cost。Correctness + dedup wins at zero hot-path cost。

**#152 concat tree single-alloc**：tree-walker eval_binary 识别 left-leaning chain → SmolStr::concat_many 单 alloc。**inline -69% / heap -59%**。Static gate is_statically_string_expr 避 dict-merge regression。

**#153 ASCII flag**：layout C (len 高 bit 复用)，header 仍 12-byte。case_fold_ascii_fast skip UCD scan。**upper/lower -86~87%**，title -22% (word-state walker serial limit)。

### Dispatch Boundary 收口

**#154 #136 carry-over (4 lever)**：
- SmallMap fast path NEW row **70.06 ns** (5× under 280 ns target)
- catch_unwind cfg-gate to debug only
- lazy trap-code reset
- entry-ptr inline cache (also code-cleanliness win)

legacy_i64 row 16 → 14.25 ns (-10.96% p<0.01)。HashMap path 366.91 → 361.99 ns 结构性受限 (HashMap+String heap alloc 不可触)。

### 架构债务清

**#156 Codegen OpVisitor**：emit_op 78 hand-rolled arms / 308 lines → walk_op(op, self) 3 lines。4 Op consumer 全统一 dispatch (bytecode CompileState / ConstPool / codegen-native::Codegen / 未来 wasm-AOT)。P0-A trait 完整接入。

### Stdlib glob_match 4 backend 闭环

**#155 glob_match bytecode + trace-JIT wire-up**：tree-walker + cranelift (#140) + bytecode (BcOp::StrGlobMatch reuse CallNative pattern) + trace-JIT (TraceOp::StrGlobMatch + HostHookId::StrGlobMatch + helper call to __relon_str_glob_match)。stdlib glob_match 现 4 backend 全接通。

## 整体性能成果

vs Wave A 完工 baseline：

| Workload | Before Wave D | After Wave D | 提升 |
|---|---:|---:|:---:|
| W12 bytecode | 447 ns (4.15× LuaJIT) | **181 ns (1.84× LuaJIT)** | **-60%** |
| W5 dict_str_key trace_jit | 168.52 µs (× 1.473) | **131.08 µs (× 1.151)** | **-22%** |
| sso/concat_short (16B) | 139 ns | **43.2 ns** | **-69%** |
| sso/concat_short (32B heap) | 248 ns | **100.7 ns** | **-59%** |
| case_fold ASCII upper | 10.85 µs | **1.40 µs** | **-87%** |
| case_fold ASCII lower | 10.88 µs | **1.48 µs** | **-86%** |
| dispatch typed-i64 | 16 ns | **14.25 ns** | **-10.96%** |
| dispatch SmallMap (new) | n/a | **70.06 ns** | NEW floor |

## 关键诚实记录

### 每 phase 留 deferred follow-up

- **#147 Lever 3 (per-op spec)**: ROI 低于工程成本，target 已超越 skip
- **#147 Lever 4 (threaded dispatch)**: Rust stable 1.95 缺 become/computed goto/unstable asm — 蓝本
- **#148 M3 phase 2**: iter stdlib (range/map/reduce/sum) body 用 buffer-protocol ops，bytecode VM 不持，需 IR desugar 或 buffer-protocol op family 后续 phase
- **#149 dict-key record only**: Op::ConstString / __relon_str_concat_alloc 未迁 (follow-up)
- **#150 trace-JIT runtime SSO**: 用 #149 StringRef records 需 separate SSO encoding (dedicated phase if hot)
- **#150 stdlib format! paths**: split/join/replace/fold/NFC 仍 build String 再 wrap，"write-to-buffer surface" Tier-2 candidate
- **#150 22 B cap ceiling**: 提升需 box heavy Value variants
- **#151 ptr-cmp shortcut**: 需 source-driven bench observable，留 follow-up
- **#152 backend wire-up**: bytecode / cranelift AOT / trace-JIT 仍 per-pair Add (larger AOT-cache change)
- **#153 evaluator→flag wiring**: fold_string surface 仍传 AsciiHint::Unknown (separate PR, evaluator string container)
- **#156 Step 4 bespoke**: Op::Select / Op::Trap / Op::Add(String) 4-8 line in visit_* body 无 second consumer，skip

## 累计 (Wave 1 + A + B+C + D)

| Wave | 任务 | LoC | tests delta |
|---|---|---:|:---:|
| Wave 1 | #121-#127 | +4622/-1043 | +22 |
| Wave A | #128-#132 | +3552/-1887 | +31 |
| Wave B+C | #133-#145 | +10,000+/-330+ | +84 |
| Wave D | #146-#156 (11 项) | +10,000+/-900+ | +66 |

**总: +28,000+/-4160+ LoC, +203 tests** (2007 → 2227)。

## 引用

stage reports (Wave D)：
- `review-improvement-{146..156}-*-2026-05-21.md` (11 stage reports)

完工文档：
- `review-improvement-completion-2026-05-21.md` (Wave 1)
- `review-improvement-wave-a-completion-2026-05-21.md` (Wave A)
- `review-improvement-wave-bc-phase4-completion-2026-05-21.md` (Wave B+C)
- `review-improvement-final-completion-2026-05-21.md` (Wave D, 本文档)

## 后续 backlog

按 ROI:

1. **M3 phase 2** iter stdlib desugar / buffer-protocol op family (解锁 cmp_lua W1-W11 bytecode 4-way)
2. **String stdlib write-to-buffer surface** (split/join/replace/fold/NFC 跳 String wrap)
3. **trace-JIT runtime SSO** (StringRef record header encode SSO)
4. **bytecode/cranelift/trace-JIT concat_many wire-up** (#152 backend deferred)
5. **Op::ConstString hash header migration** (#149 dict-key only deferred)
6. **evaluator → ASCII flag wiring** (#153 fold_string surface 传 hint)
7. **#147 Lever 3 per-op spec / Lever 4 threaded dispatch** (need stable Rust feature)

长期：
- **W11 musl static-link** RFC-class
- **W5 perfect-hash ≥ 5-entry** (current × 1.151 margin 大，无紧迫)
- **Cranelift 0.131 升级** (long-term)
- **Regex Tier 3 (RE2-linear)** (no demand)

## 结论

11 项 Wave D 全完，无 correctness regression。每 phase 全 5 gate 过（fmt / clippy / test / wasm32 / corpus three_way）。所有 deferred 项有 stage report 蓝本。

Tests 2161 → **2227** (+66)。bytecode W12 1.84× LuaJIT (was 4.15×)。W5 dict × 1.151 (was × 1.473)。SSO concat -82%。ASCII fold -87%。Codegen OpVisitor 4 backend 统一。glob_match 4 backend 全接通。

× 1.5 全维度 stretch target 大幅超越。Bytecode VM 从 scaffold + scaffold extension 推到 close LuaJIT (1.84×) + closure surface + 4-backend OpVisitor 接入。String performance 从 LuaJIT 借鉴 5 个 lever 全部落地，覆盖 hash cache + SSO + intern + ASCII fast-path + concat tree。
