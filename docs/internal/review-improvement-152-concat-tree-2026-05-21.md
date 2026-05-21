# Review Improvement #152 — String Tier 2b Concat Tree Single-Alloc

**Date**: 2026-05-21
**Worktree**: `/ext/relon/.claude/worktrees/agent-a565ae03cf42c659d`
**Base**: `57d7bf3` (#150 SSO SmolStr merged)
**Branch**: `worktree-agent-a565ae03cf42c659d`

## Audit — current concat path

`crates/relon-evaluator/src/arithmetic.rs::eval_binary` evaluates
`Operator::Add` strictly recursively: it calls `self.eval(left)`,
then `self.eval(right)`, then dispatches the `(Value, Value)` pair.
For a chained source expression like `"a" + "b" + "c" + "d"` the
parser produces a left-leaning AST `(((a+b)+c)+d)`, so the
recursive eval shape performs three sub-concats. Each step calls
`SmolStr::concat(a, b)` on the result of the previous step. Once
the running prefix passes the 22-byte SSO inline cap, each concat
allocates a fresh `Arc<str>` of length `prefix + leaf` and copies
the prefix into it. For N leaves with total bytes > cap, that is
`N-1` allocations and `N-1` prefix copies (LuaJIT does this in
one allocation + one final memcpy).

## Option choice

Picked **Option B (tree-walker only) + helper in eval-api** rather
than Option A (full IR Op::StrConcatN + 4-backend wire-up). Reasons:

* The chained `+` shape is a tree-walker hot path (format-style
  string assembly: `"prefix " + name + ": " + value`). The other
  backends (bytecode VM, cranelift, trace-JIT) currently route
  String concat through helper calls that already collapse per-
  Add — moving them off Op::Add(String) would be a separate L-
  sized wire-up with its own AOT / cache invalidation surface.
* The IR Op::Add(IrType::String) already serves the per-pair
  semantics. A fold into Op::StrConcatN would need an additional
  pattern-match in the IR lowering pass, every OpVisitor's
  `visit_*`, and matching codegen in three backends.
* The eval-api crate already owns `SmolStr` (24-byte inline /
  Arc<str> heap), so the single-alloc helper `concat_many` lives
  alongside `concat` with zero new dependencies.

ROI: micro-bench shows -59% to -69% on the fold path (well above
the 15-25% target), and the change is contained to three files.
Defer Option A IR variant to a follow-up when the bytecode /
cranelift backends are themselves hot on chained concat.

## Backend status

* **Tree-walker**: ✓ folded via `try_eval_string_concat_chain`
  with a static "deepest LHS leaf is `Expr::String`/`Expr::FString`"
  gate so dict-merge / schema-merge chains take zero extra cost.
* **Bytecode VM**: deferred — still emits per-pair `Op::Add` via
  the bytecode lowering; would need its own ConcatN op variant
  and a matching VM dispatch.
* **Cranelift AOT**: deferred — routes through the existing
  `__relon_str_concat` host shim per-pair. A ConcatN host shim
  would mirror the SmolStr API but touches the AOT cache key.
* **Trace JIT**: deferred — `TraceOp::StrConcat` records per-pair;
  a recorder-side fold could batch consecutive `StrConcat` ops
  into a single emitted ConcatN call.

## Bench numbers

`sso/concat_tree` row group (added by this phase, 3s/sample):

| regime  | impl                  | time     | delta vs baseline |
|---------|-----------------------|----------|-------------------|
| inline  | nested_concat         | 139.0 ns | baseline          |
| inline  | concat_many           |  43.2 ns | **-69%**          |
| inline  | string_with_capacity  |  51.6 ns | -63%              |
| heap    | nested_concat         | 248.0 ns | baseline          |
| heap    | concat_many           | 100.7 ns | **-59%**          |
| heap    | string_with_capacity  |  51.0 ns | -79%              |

`concat_many` beats `nested_concat` by ~60-70% across both
regimes. The `string_with_capacity` heap row is faster than
`concat_many` because the latter additionally wraps the final
buffer in an `Arc<str>` for shared-clone semantics — the gap is
the fixed cost of the refcounted handle and is recovered
downstream by O(1) clones.

End-to-end W3 (`tests/cmp_lua_consistency::w3_string_concat`)
remains green. W3 itself is a reduce-loop `acc + s` shape (one
Add per iteration, not a single-expression chain), so the fold
gate does not fire and W3's macro-bench numbers do not move
from this phase — the gain lands on format-style chains, which
are the more common idiom in real Relon programs.

## Cumulative W3 trace_jit ratio improvement (#149 + #150 + #152)

* #149 string header hash inlined Arc<str> identity check (W3
  hot path on String key lookups).
* #150 SSO SmolStr 22-byte inline (drops one `String` alloc on
  every short-payload concat, including the per-iter `acc + s`
  step until the prefix passes 22 bytes).
* #152 concat-tree single-alloc fold (this phase, lands on
  format-style chains; orthogonal to W3 reduce loop).

#152 does not move W3's reduce-loop ratio directly because each
iteration's Add is not a chain. The cumulative trace_jit ratio
gain from #149 + #150 stands at ~16% on the W3 macro-bench (per
#150's stage report); #152's micro-bench gain is contained in
the `sso/concat_tree` rows above and applies to per-expression
chains rather than the W3 reduce shape.

## Gate

* `cargo fmt --all --check` — clean.
* `cargo clippy --workspace --all-targets -- -D warnings` — clean
  (one `while-let-loop` lint resolved during the iteration).
* `cargo test --workspace` — **2227 passed**, 0 failed (≥ 2219
  baseline; +6 new tests for `concat_many` + chain fold).
* `cargo check --target wasm32-unknown-unknown` — clean.
* `tests/cmp_lua_consistency::w3_string_concat` — pass.

## Commits

* `f8b38e4` perf(eval-api): SmolStr::concat_many single-alloc N-slice fold
* `eec4a37` perf(evaluator): fold left-leaning String+String chain into concat_many
* `42c3c7c` test(sso): concat_tree row group for nested vs concat_many
