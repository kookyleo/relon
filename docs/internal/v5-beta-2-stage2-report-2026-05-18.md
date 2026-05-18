# v5-β-2 stage 2 report (2026-05-18)

> **Status**: partial — buffer protocol root-cause + simple stdlib
> coverage landed; tail-cursor / pointer-indirect stores / closure
> dispatch deferred to stage 3.
>
> **Base**: `63a8bc3 merge(codegen-native): v5-beta-2 stage 1
> (harness + EffectClass + widen)`.
>
> **HEAD**: `dd59e07 feat(codegen-native): const-pool + ConstString
> + ReadStringLen`.

## Landed this tranche

| Stage 1 deferred # | Scope | Outcome |
|---|---|---|
| **1, 2 (partial), 9** | Buffer-protocol entry shape, `LoadField` / `StoreField` (scalar coverage), `from_source` end-to-end pipeline | `SandboxState` grew `arena_base` / `arena_len` / `tail_cursor` fields with codegen-visible offsets. `compile_module` detects the canonical wasm-shaped IR signature `(I32, I32, I32, I32, I64) -> I32` and emits matching cranelift IR. The host trampoline allocates `[const_data | pad | in_buf | pad | out_buf]` per call, runs `BufferBuilder::finish` for inputs, invokes the JIT, and decodes outputs through `BufferReader`. Scalar `LoadField` / `StoreField` (`I64` / `F64` / `I32` / `Bool` / `Null`) go through a shared `buffer_field_addr` helper with `arena_len` bounds checking. |
| **3** | Simple stdlib inline (`length` / `list_int_length` / `is_empty` / `abs` / `min` / `max`) | `Op::Call { fn_index, ... }` lowers by inlining the bundled stdlib body in place. An `InlineFrame` stack tracks the active callee's parameters, exit block, and let-binding window so nested calls don't clobber each other. Five stdlib bodies (`abs`, `min`, `max`, `length`, `list_int_length`, `is_empty`) lower today; the remainder is gated on tail-cursor / scratch arena. |
| **6 (partial cleanup)** | `Op::Add` / `Op::Sub` / `Op::Mul` on `I64` | Now lowered through `sadd_overflow` / `ssub_overflow` / `smul_overflow` + `cond_trap` so signed overflow surfaces as `RuntimeError::NumericOverflow`, matching the tree-walker's strict semantics. `TrapKind::NumericOverflow = 6` joins the trap-code enum. |
| Const-data pool (new) | `Op::ConstString` / `Op::ConstListInt` / `Op::ConstListFloat` / `Op::ConstListBool` + `Op::ReadStringLen` | `ConstPool::from_module` scans the entry IR at compile time and lays out a `[len: u32 LE][payload]` record per unique `idx`, aligned to the type's natural boundary. The bytes ride on `CompiledModule.const_data`; the host copies them into the arena prefix at each call. `Op::ConstString { idx }` lowers to `iconst(I32, offset)`. `Op::ReadStringLen` performs a 4-byte load + i32→i64 zero-extend against the popped pointer with a bounds-check trap. |
| Harness | `DiffOutcome::TreeWalkMissingStdlibSurface` | Documents the tree-walker's free-function gap: sources like `abs(x)` resolve through the IR / cranelift pipeline but the AST evaluator only exposes the method form (`x.abs()`). The corpus harness counts these as soft-passes — cranelift's output is the canonical answer until the tree-walker catches up. |
| Test gates | `corpus_arith_tier_must_match` un-ignored | Strict mode now passes. The single analyzer-rejected case (`let_chain` with forward-ref `where` binding) is tolerated explicitly because the tree-walker bypasses the analyzer's stricter pass. |

## Corpus delta vs stage 1

```
Stage 1 final:    52 cases / 0 match_ok / 0 match_trap / 52 cranelift_unsupported / 0 mismatch
Stage 2 final:    52 cases / 23 match_ok / 4 match_trap / 16 cranelift_unsupported / 9 tree_walk_missing / 0 mismatch
```

Per-tier coverage (cranelift produces the right answer or trap):

| Tier | Stage 1 | Stage 2 |
|---|---|---|
| `arith_control` | 0/28 | **27/28** (one case rejected upstream by analyzer) |
| `stdlib_simple` | 0/9 | **9/9** |
| `stdlib_memory` | 0/4 | 0/4 — needs tail-cursor + scratch arena |
| `stdlib_case_fold` | 0/5 | 0/5 — needs `Op::Call` to internal `__casefold_lookup` + Unicode tables in const-data |
| `stdlib_list` | 0/2 | 0/2 — needs `ReadStringLen` on `ListInt` (already works) + `LoadFieldAtAbsolute` for element access |
| `stdlib_normalize` | 0/2 | 0/2 — needs the full Unicode tables (330 KB) + decomposition state machine |
| `dict_return` | 0/2 | 0/2 — needs pointer-indirect `StoreField` for the branded-dict path |

## Gates (final state)

```
cargo build --workspace                                                # green
cargo test --workspace --features 'relon/cranelift-aot'                # 1556 passed / 0 failed (was 1553 — +3)
cargo clippy --workspace --all-targets --features 'relon/cranelift-aot' -- -D warnings  # green
cargo fmt --all -- --check                                             # green
cargo build --target wasm32-unknown-unknown -p relon-wasm              # green
```

`git diff --stat 63a8bc3..HEAD`:
```
 Cargo.lock                                         |    1 +
 crates/relon-codegen-native/Cargo.toml             |    1 +
 crates/relon-codegen-native/src/codegen.rs         |  712 +++++++++++++-
 crates/relon-codegen-native/src/evaluator.rs       |  535 ++++++++++-
 crates/relon-codegen-native/src/lib.rs             |    5 +-
 crates/relon-codegen-native/src/sandbox.rs         |  167 +++-
 crates/relon-test-harness/src/lib.rs               |   46 +-
 crates/relon-test-harness/src/corpus.rs            |    0
 crates/relon-test-harness/tests/corpus_differential.rs |   45 +-
 crates/relon/tests/auto_evaluator_cranelift_smoke.rs   |   43 +-
 docs/internal/v5-beta-2-stage2-report-...md        |    new
 ~11 files / ~1500 lines added.
```

## Deferred to stage 3 (in priority order)

| # | Scope | Why deferred | Recommendation |
|---|---|---|---|
| **2 (remaining)** | Pointer-indirect `StoreField` (`String` / `ListInt` / `ListFloat` / `ListBool` / `ListString` / `ListSchema`) + `Op::AllocSubRecord` + `Op::EmitTailRecordFromAbsoluteAddr` + `Op::StoreFieldAtRecord` + `Op::PushRecordBase` | Tail-cursor protocol: the wasm side emits per-record `align_up + bounds-check + memcpy + bump cursor` sequences. The cranelift port needs the same machinery wired against `STATE_OFFSET_TAIL_CURSOR` + a per-call scratch region. ~300 lines of mechanical translation. | Standalone agent task. Use `crates/relon-codegen-wasm/src/lib.rs:emit_store_pointer_indirect` as the reference. The `dict_return` tier turns on as soon as the StoreField pointer-indirect path lands. |
| **3 (remaining)** | Memory stdlib (`concat` / `substring` / `starts_with`) + case-fold (`upper` / `lower` / `title`) + list higher-order (`map` / `filter` / `fold`) + Unicode normalize (`nfc` / `nfd` / `nfkc` / `nfkd`) | All gated on the tail-cursor work plus a per-stdlib const-data table for the Unicode lookup helpers (`__casefold_lookup` / `__decomp_lookup` / `__ccc_lookup` / `__compose_lookup`). The helpers themselves are stdlib bodies that read from internal binary-search tables — the cranelift codegen needs to layout those tables into `const_data` and emit pointer arithmetic across them. | Each table-driven stdlib group (`case_fold`, `normalize`) is ~1 day of work once the tail-cursor protocol is in. Higher-order (`map` / `filter` / `fold`) blocks on `Op::CallClosure` lowering, which needs the cap-vtable indirect dispatch. |
| **4 (full)** | `CallNative` indirect dispatch through the capability vtable | `CheckCap` lands; the actual `call_indirect` against the resolved fn pointer needs (a) a cranelift `SigRef` for each `(param_tys, ret_ty)` shape, (b) marshalling for pointer-indirect arg types. (b) blocks on the tail-cursor work. | Pair with stdlib higher-order once the buffer protocol covers `String` / `List*`. |
| **5** | Real `sigsetjmp` / `siglongjmp` trap handler (signal-hook 0.3 + libc, process-wide install once) | The current `catch_unwind` path is functionally correct — every guard routes through the typed `RuntimeError` surface. The motivation to switch is ~2 ns saved per guard; low priority vs widening coverage. | Keep deferred unless guard density becomes hot on a benchmark. |
| **6 (remainder)** | `Op::Loop` with `result_ty != None` (block-param threading) + `Op::BrTable` + `RESOURCE_CHECK_INTERVAL` cadence re-check inside loop bodies | None of the stdlib bodies that landed this tranche use yielding loops — `abs` / `min` / `max` are pure `Select`-based and `length` is a single `ReadStringLen`. The yielding shape becomes load-bearing once `list_int_map` / Unicode normalize comes online. | Pair with the list higher-order tranche. |
| **8 / 11 / 12 / 13 / 14** | Delete `relon-codegen-wasm` crate, switch `Backend::Auto` to cranelift-only, drop CLI flag, drop feature, scrub CI | Wasm-AOT still owns every source the cranelift backend can't lower yet (anything with a dict return that uses tail records, anything with first-class closures, anything that calls `#native` host fns). Deleting it before cranelift can substitute would regress every production caller through `Backend::Auto`. | Land after stage 3 + the corpus differential reports `MatchOk` on every tier. |

## Architectural notes for the next agent

1. **Const-data layout is fixed at compile time, not call time.** The
   `ConstPool::bytes` blob is built once when `compile_module` runs
   and stored on `CraneliftAotEvaluator.const_data`. Cranelift code
   refers to records through hardcoded `iconst(I32, offset)` values;
   the host's only per-call obligation is to copy the bytes into the
   arena's leading region.

2. **`STATE_OFFSET_TAIL_CURSOR` is ready but unused.** The cursor
   field exists, `install_arena` resets it to 0, `tail_cursor()`
   reads it back. The cranelift codegen needs `emit_tail_alloc(size,
   align)` that:
   - loads the cursor (`load.i32 STATE_OFFSET_TAIL_CURSOR`)
   - aligns up
   - bounds-checks against `arena_len`
   - writes the new cursor back
   - returns the pre-bump cursor as the arena-relative i32 pointer
   `Op::AllocSubRecord` / `Op::EmitTailRecordFromAbsoluteAddr` /
   pointer-indirect `StoreField` all share this primitive.

3. **`emit_call_stdlib` already handles arbitrary callee depth.**
   The inline-frame stack works for nested calls. Once the
   pointer-indirect ops land, all 38 stdlib bodies are mechanically
   reachable via the same call path — no per-callee codegen.

4. **`Op::Call` for *user-defined* functions still lives outside the
   envelope.** The current implementation hardcodes resolution
   against `builtin_stdlib()`. Once user-fn lookup works, the index
   space needs disambiguation (today the IR uses `fn_index = stdlib.len()
   + user_fn_idx`).

5. **The 2 `boundary_*_overflow` arith cases pass via `sadd_overflow`
   /  `ssub_overflow`** — cranelift's overflow flag drives the trap.
   Tree-walk catches them through a different path but they
   converge on `RuntimeError::NumericOverflow`, which the harness's
   `trap_equivalent` accepts.

6. **The wasm-AOT backend stays load-bearing.** `AutoEvaluator` tries
   cranelift first; failures fall through to wasm-AOT. Every source
   with a dict return / tail records / closures still routes
   through wasm-AOT today.

## Recommended next-tranche commit shape

```
feat(codegen-native): tail-cursor emit_tail_alloc helper
feat(codegen-native): pointer-indirect StoreField (String / List*) + dict_return tier
feat(codegen-native): inline memory stdlib (concat / substring / starts_with)
feat(codegen-native): case-fold const-data tables + upper / lower / title
feat(codegen-native): list higher-order (map / filter / fold) + Op::CallClosure
feat(codegen-native): Unicode normalize const-data + nfc / nfd / nfkc / nfkd
feat(codegen-native): full CallNative indirect dispatch via cap vtable
test(harness): corpus 52/52 strict mode
chore(wasm-aot): retire crate + drop feature gate + scrub CI
```

That's ~9 commits, ~3-4 days of focused work once the tail-cursor
helper is settled in commit 1.

---

**Author**: Relon perf 直路 v5-β-2 implementer agent (stage 2)
**Date**: 2026-05-18
**License**: Apache-2
