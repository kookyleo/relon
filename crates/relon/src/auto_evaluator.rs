//! Auto-tier evaluator wrapper.
//!
//! Landing surface for [`crate::Backend::Auto`]. An [`AutoEvaluator`]
//! eagerly constructs a [`TreeWalkEvaluator`] (cheap, ~1 ms) and
//! lazily initialises the cranelift-AOT backend only when a host
//! first calls [`relon_eval_api::Evaluator::run_main`]. The other
//! four `Evaluator` methods (`eval`, `eval_root`, `force_thunk`,
//! `invoke_closure`) always go through the tree-walker — which is
//! also the only backend that supports them today.
//!
//! ## Design notes
//!
//! * **Lazy AOT init** keeps the wasted-work scenario (host parses,
//!   reads static config via `eval_root`, never calls `run_main`)
//!   free of any AOT compile cost.
//! * **Thread-safe + cache-once.** Both the success and failure
//!   paths are cached in `OnceLock`s, so concurrent `run_main`
//!   callers only ever drive the AOT pipeline once.
//! * **AOT setup failure isolation.** If the cranelift-AOT pipeline
//!   fails (build compiled without `cranelift-aot`, source uses a
//!   construct the AOT backend rejects, ...), only `run_main`
//!   returns an error; `eval` / `eval_root` / `force_thunk` /
//!   `invoke_closure` keep working off the tree-walker.
//! * **`Box<dyn Evaluator>`** is stored rather than the concrete
//!   `AotEvaluator` so future backends can swap in without
//!   changing this file's surface.
//!
//! v5-β-2 stage 4: wasm-AOT retired here. The fallback path that
//! used to try cranelift then drop to wasm-AOT is gone — the
//! cranelift backend now covers every IR op the corpus exercises,
//! so wasm-AOT was dead weight.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_parser::{Expr, Node, Operator};

use crate::BackendError;

/// Wrapper [`Evaluator`] that routes the four AST-aware methods
/// through a tree-walker and lazily spins up the cranelift-AOT
/// backend on first `run_main`. Constructed via [`AutoEvaluator::new`]
/// or [`crate::new_evaluator`] with [`crate::Backend::Auto`].
pub struct AutoEvaluator {
    /// Source kept around so we can drive the AOT pipeline on first
    /// `run_main`. Owned rather than borrowed so the wrapper outlives
    /// the caller's source string.
    source: String,
    /// Eagerly-constructed tree-walker. Owned via a `Box` so the
    /// wrapper stays `Send + Sync` without bounding `TreeWalkEvaluator`
    /// generics into this file.
    tree_walk: Box<TreeWalkEvaluator>,
    /// Lazy cranelift-AOT backend; once the first `run_main` runs
    /// the codegen pipeline successfully, every subsequent call
    /// reuses this slot. Stored as `Box<dyn Evaluator>` so future
    /// backends can swap in without touching this struct's layout.
    aot: OnceLock<Box<dyn Evaluator>>,
    /// Mirror slot for the failure path. If the AOT pipeline fails
    /// once we cache the error so repeated `run_main` calls don't
    /// each pay a fresh parse / analyze / lower / codegen attempt.
    /// Carries a classification flag (`unsupported_shape`) so
    /// `run_main` can distinguish "compiled backend can't express this
    /// shape" (fall back to tree-walk) from a genuine source / host
    /// error (surface it).
    aot_init_err: OnceLock<AotInitError>,
    /// v6-fix-D2 default-path: set when the `#main` shape is
    /// trivial scalar — a single scalar-typed parameter plus a
    /// single literal-or-arithmetic body. On such shapes the
    /// cranelift-AOT lower plus JIT cold-start (~4.7 ms on the
    /// W11 source) is pure overhead; the tree-walker answers in
    /// ~60 µs. Set once at [`Self::new`] time after a cheap AST
    /// inspection and read on every [`Self::run_main`] dispatch
    /// to short-circuit straight into [`Self::tree_walk`] without
    /// ever building the AOT pipeline. The judgment is
    /// intentionally conservative: any closure, list, dict,
    /// comprehension, match, where, f-string, fn-call or
    /// multi-parameter shape falls through to the normal AOT
    /// route so workloads that benefit from JIT keep paying the
    /// cost only on the first call (and the cache fast-restore
    /// covers subsequent ones).
    is_trivial_main: bool,
}

/// Cached outcome of a failed AOT pipeline build.
///
/// The `message` reproduces the original error text for diagnostics;
/// `unsupported_shape` records whether the failure was a compiled-backend
/// *capability boundary* (the shape can't be lowered / codegen'd — auto
/// then falls back to the tree-walk oracle) rather than a genuine source
/// or host / infrastructure error (which `run_main` surfaces verbatim).
struct AotInitError {
    /// Original error text, surfaced unchanged when this is not a
    /// shape-capability failure.
    message: String,
    /// `true` only for compiled-backend capability-boundary errors
    /// (lowering / codegen / unsupported `#main` signature). See
    /// [`relon_codegen_cranelift::CraneliftError::is_unsupported_shape`].
    unsupported_shape: bool,
}

impl AutoEvaluator {
    /// Build an [`AutoEvaluator`] over `source`. The tree-walker is
    /// constructed eagerly — same pipeline `Backend::TreeWalk`
    /// uses — so the wrapper is immediately ready to serve `eval` /
    /// `eval_root` / `force_thunk` / `invoke_closure`. The AOT
    /// half stays unbuilt until the first `run_main` invocation.
    pub fn new(source: &str) -> std::result::Result<Self, BackendError> {
        // Parse exactly once, hand the same AST to both the trivial
        // classifier (borrow) and the tree-walker build (move). v6-fix-
        // D2-default originally re-parsed inside `is_trivial_scalar_main`,
        // which doubled cold-start parse cost. Now the source is parsed
        // here and the borrow runs the same classifier rules.
        let node =
            relon_parser::parse_document(source).map_err(|e| BackendError::Parse(e.to_string()))?;
        let is_trivial_main = is_trivial_scalar_main_node(&node);
        let tree_walk = crate::build_tree_walk_evaluator_from_parsed(node)?;
        Ok(Self {
            source: source.to_string(),
            tree_walk: Box::new(tree_walk),
            aot: OnceLock::new(),
            aot_init_err: OnceLock::new(),
            is_trivial_main,
        })
    }

    /// Test / observability hook: returns `true` when [`Self::new`]
    /// classified the source as a trivial scalar `#main` (single
    /// scalar param + scalar literal / arith body). Used by the
    /// auto_evaluator smoke tests to assert the conservative
    /// classifier doesn't misjudge complex shapes.
    pub fn is_trivial_main(&self) -> bool {
        self.is_trivial_main
    }

    /// Returns `true` if the cranelift-AOT backend has been
    /// constructed (either successfully or with a cached error).
    /// Exposed for tests / observability so a host can assert that
    /// lazy init actually stayed lazy across an `eval` / `eval_root`
    /// path.
    pub fn is_aot_initialised(&self) -> bool {
        self.aot.get().is_some() || self.aot_init_err.get().is_some()
    }

    /// Reach the lazy AOT backend, building it on demand. Returns
    /// the cached error reference if a prior call already failed —
    /// the AOT pipeline is deterministic, so retrying after a failure
    /// would just burn CPU on the same error.
    fn aot(&self) -> std::result::Result<&dyn Evaluator, &AotInitError> {
        // Fast path: already built successfully.
        if let Some(aot) = self.aot.get() {
            return Ok(aot.as_ref());
        }
        // Fast path: already failed; surface cached error without
        // re-running the pipeline.
        if let Some(err) = self.aot_init_err.get() {
            return Err(err);
        }
        // Slow path: try to build. Multiple concurrent callers may
        // race here, but `OnceLock::set` will only let one entry in;
        // the loser falls through to the fast-path on the next
        // iteration via the re-check below.
        match Self::build_aot(&self.source) {
            Ok(aot) => {
                // Ignore the `Err(_)` from `set` — that just means
                // another thread won the race, in which case its
                // value (semantically identical) is already in the
                // slot. We re-read so the returned reference points
                // at the winning entry.
                let _ = self.aot.set(aot);
                Ok(self
                    .aot
                    .get()
                    .expect("aot slot populated after set / race-loss")
                    .as_ref())
            }
            Err(err) => {
                let _ = self.aot_init_err.set(err);
                Err(self
                    .aot_init_err
                    .get()
                    .expect("aot_init_err slot populated after set / race-loss"))
            }
        }
    }

    /// Drive the AOT pipeline over `source`.
    ///
    /// v5-β-2 stage 4: cranelift-AOT is the only AOT backend left.
    /// When the `cranelift-aot` feature is enabled this returns the
    /// boxed evaluator; otherwise the slot caches a "feature off"
    /// error and `run_main` returns `RuntimeError::Unsupported`.
    ///
    /// v5-γ: try the on-disk cache first (saves parse + analyze +
    /// lower on the second cold start), then fall back to
    /// `from_source_with_cache` which writes a fresh cache as a
    /// side-effect. Cache directory is selected via
    /// `relon_codegen_cranelift::default_cache_dir()`; hosts that want
    /// to override (test isolation, embedded targets) can override
    /// the `XDG_CACHE_HOME` / `HOME` env vars before constructing
    /// the evaluator.
    #[cfg(feature = "cranelift-aot")]
    fn build_aot(source: &str) -> Result<Box<dyn Evaluator>, AotInitError> {
        let cache_dir = relon_codegen_cranelift::default_cache_dir();

        // 1. Cache-hit fast path: pull a (source, sandbox)-keyed
        // pair off disk and reconstruct the JIT module from the IR
        // half. Any soft miss (file absent, integrity failure,
        // metadata mismatch) surfaces as `Ok(None)`; only an
        // unexpected I/O failure escapes here.
        match relon_codegen_cranelift::AotEvaluator::from_cache_dir(source, &cache_dir) {
            Ok(Some(aot)) => return Ok(Box::new(aot) as Box<dyn Evaluator>),
            Ok(None) => {} // cache miss — proceed to source build
            Err(e) => {
                // Cache load infrastructure problem (not a miss).
                // Log via tracing and continue to the source path so
                // a transient cache issue doesn't break the live
                // invocation.
                tracing::warn!(
                    target: "relon::auto_evaluator",
                    "cache load failed: {e}; falling back to from_source"
                );
            }
        }

        // 2. Cache-miss path: full pipeline, writes fresh cache pair
        // as a side-effect so the *next* cold start can hit.
        relon_codegen_cranelift::AotEvaluator::from_source_with_cache(source, &cache_dir)
            .map(|aot| Box::new(aot) as Box<dyn Evaluator>)
            .map_err(|e| AotInitError {
                // Classify before stringifying: only compiled-backend
                // capability-boundary errors (lowering / codegen /
                // unsupported `#main` signature) authorise a tree-walk
                // fallback. Parse / analyze (source errors) and host /
                // infra faults keep `unsupported_shape = false` so
                // `run_main` surfaces them.
                unsupported_shape: e.is_unsupported_shape(),
                message: e.to_string(),
            })
    }

    /// Stub for builds compiled without `cranelift-aot` (e.g. wasm32
    /// hosts). `run_main` surfaces the cached error; the tree-walker
    /// surface keeps working.
    #[cfg(not(feature = "cranelift-aot"))]
    fn build_aot(_source: &str) -> Result<Box<dyn Evaluator>, AotInitError> {
        // "Feature off" is a build-configuration fault, not a shape
        // capability boundary — but the tree-walk surface is the only
        // backend in this build anyway, so `run_main` already routes
        // there. Keep `unsupported_shape = false`; it never gets read on
        // this cfg because there is no compiled backend to fall back
        // *from*.
        Err(AotInitError {
            message: "this build was compiled without the `cranelift-aot` feature; rebuild with `--features cranelift-aot` to enable the AOT backend"
                .to_string(),
            unsupported_shape: false,
        })
    }
}

impl Evaluator for AutoEvaluator {
    fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Tree-walker is the only backend that exposes arbitrary
        // node evaluation today; routing here keeps `eval` cheap
        // even if a host later mixes it with `run_main`.
        self.tree_walk.eval(node, scope)
    }

    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Static-config / library-mode path; tree-walker always.
        self.tree_walk.eval_root(scope)
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // v6-fix-D2 default-path: trivial scalar `#main` shapes
        // (single Int/Float/Bool/Null/String parameter + literal /
        // arith body) skip the cranelift-AOT init entirely. The
        // tree-walker reaches the same answer in microseconds on
        // these shapes; the cranelift lower + JIT codegen cost
        // (~4-5 ms cold) is pure overhead. See [`Self::new`] for
        // the conservative classifier.
        if self.is_trivial_main {
            tracing::debug!(
                target: "relon::auto_evaluator",
                "trivial scalar #main; routing run_main through tree-walker (skipping cranelift-AOT init)"
            );
            // `Evaluator::run_main` here is the trait method on
            // `TreeWalkEvaluator` (qualified dispatch so we don't
            // accidentally hit the inherent impl, which takes an
            // explicit `&Arc<Scope>`). The trait variant builds the
            // shared empty scope from the evaluator's own
            // `empty_scope()`, identical to what
            // `cranelift-AOT`'s `run_main` does on the AOT side.
            return Evaluator::run_main(self.tree_walk.as_ref(), args);
        }
        match self.aot() {
            Ok(aot) => aot.run_main(args),
            // The compiled backend can't express this `#main` shape
            // (e.g. a `-> List<P>` return). That's a capability
            // boundary, not a program error — adapt by running the
            // tree-walk oracle, which handles every such shape and
            // produces the authoritative result. The trade-off is the
            // loss of AOT acceleration for this run, so we log it.
            Err(err) if err.unsupported_shape => {
                tracing::info!(
                    target: "relon::auto_evaluator",
                    reason = %err.message,
                    "compiled (cranelift-AOT) backend can't express this #main shape; \
                     falling back to the tree-walk interpreter — this run forgoes AOT acceleration"
                );
                Evaluator::run_main(self.tree_walk.as_ref(), args)
            }
            // Genuine source error (parse / analyze) or host /
            // infrastructure fault (JIT setup, module define, cache
            // I/O). Surfacing it keeps real problems visible rather
            // than masking them behind a silent tree-walk run.
            Err(err) => Err(RuntimeError::Unsupported {
                reason: format!("auto backend: cranelift-AOT setup failed: {}", err.message),
            }),
        }
    }

    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        self.tree_walk.force_thunk(thunk)
    }

    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError> {
        self.tree_walk.invoke_closure(closure, args)
    }
}

/// v6-fix-D2 default-path classifier. Returns `true` when `source`
/// declares a `#main(...)` whose params are all scalars and whose body
/// is a single literal-or-arithmetic expression. Used by
/// [`AutoEvaluator::new`] to decide whether the cold-start cranelift
/// pipeline (~4-5 ms) can be skipped in favour of the tree-walker
/// (~60 µs on the same shape).
///
/// Re-exported from [`crate`] as
/// [`crate::is_trivial_scalar_main`] so hosts (specifically the
/// `relon-cli` driver) can run the same classifier on their own
/// pre-parsed sources and route the short-circuit straight through
/// their workspace-built tree-walker — saving the duplicate parse +
/// analyze that `AutoEvaluator::new` would otherwise run.
///
/// Conservative judgment rules — any rejection falls through to the
/// normal AOT path:
///
/// * Source must parse cleanly. Re-parsing here is cheap (the
///   tree-walker build already ran the same parse and would have
///   surfaced the error); we just demote a parse failure to
///   "not trivial" so the AOT pipeline's own error reporting can
///   surface the real diagnostic on first `run_main`.
/// * Exactly one `#main(...)` directive must be present, with no
///   `#import`s on the entry. `#import` could pull in non-trivial
///   methods / closures via the import graph; we don't trace
///   transitively.
/// * Every `#main` parameter type must be a single-segment builtin
///   scalar (`Int` / `Float` / `Bool` / `Null` / `String`). Tuples,
///   generics, lists, dicts, schemas, branded types — all disqualify.
/// * The body expression's shape must be a literal (Int / Float /
///   Bool / Null / String), a `Variable` reference, a `Reference`,
///   a `Unary` over a trivial leaf, a `Binary` whose both operands
///   are also trivial leaves (recursive), or a `Ternary` whose three
///   sub-nodes are trivial leaves. F-strings, fn calls, closures,
///   list / dict literals, comprehensions, schema variant ctors,
///   match arms, where bindings all disqualify — those rely on
///   environment / stdlib / capture sites that the tree-walker
///   handles correctly but with materially different cost than the
///   trivial-arithmetic case.
pub fn is_trivial_scalar_main(source: &str) -> bool {
    // Parse failures are demoted to "not trivial" so the AOT path's
    // own diagnostic surfaces verbatim on first `run_main`. Callers
    // that already hold a parsed `Node` (e.g. `AutoEvaluator::new`)
    // skip this parse via [`is_trivial_scalar_main_node`].
    let Ok(root) = relon_parser::parse_document(source) else {
        return false;
    };
    is_trivial_scalar_main_node(&root)
}

/// AST-input variant of [`is_trivial_scalar_main`]. Used by
/// [`AutoEvaluator::new`] to run the classifier on an already-parsed
/// document, avoiding the ~100-300 µs duplicate parse cost the
/// original source-string entry point pays.
pub fn is_trivial_scalar_main_node(root: &relon_parser::Node) -> bool {
    use relon_parser::DirectiveBody;
    // Exactly one `#main(...)` directive, no `#import`s. Multi-main
    // is an analyzer error anyway; we early-return false rather than
    // racing the analyzer.
    let mut main_directive = None;
    for dir in &root.directives {
        if dir.name == "main" {
            if main_directive.is_some() {
                return false;
            }
            main_directive = Some(dir);
        }
        if dir.name == "import" {
            return false;
        }
    }
    let Some(dir) = main_directive else {
        return false;
    };
    let DirectiveBody::Main { params, .. } = &dir.body else {
        return false;
    };
    // Every parameter type must be a single-segment scalar builtin.
    // Generics / optionals / variants all disqualify so the
    // tree-walker's scalar arg-binding path is the only one that
    // can apply.
    for p in params {
        if !is_scalar_builtin_type(&p.type_node) {
            return false;
        }
    }
    // Body must be a trivial leaf or arithmetic over trivial leaves.
    // `&*root.expr` is the entry's root expression node.
    is_trivial_body(&root.expr)
}

/// Predicate over a `TypeNode` shape: single-segment, no generics,
/// no `is_optional`, no `variant_fields`, head ∈
/// {`Int`, `Float`, `Bool`, `Null`, `String`}. Used by
/// [`is_trivial_scalar_main`] to gate trivial-main classification on
/// the parameter shape.
fn is_scalar_builtin_type(t: &relon_parser::TypeNode) -> bool {
    if t.is_optional || t.variant_fields.is_some() {
        return false;
    }
    if t.path.len() != 1 {
        return false;
    }
    if !t.generics.is_empty() {
        return false;
    }
    matches!(
        t.path[0].as_str(),
        "Int" | "Float" | "Bool" | "Null" | "String"
    )
}

/// Predicate over an expression body: returns `true` for the closed
/// set of shapes the tree-walker can answer in ~60 µs without
/// touching closure / list / dict / stdlib / capture machinery. See
/// [`is_trivial_scalar_main`] for the precise rule set.
fn is_trivial_body(expr: &Expr) -> bool {
    match expr {
        // Pure literal leaves — no environment touch.
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::String(_) => true,
        // Single-segment variable read (e.g. `x` from `#main(Int x)`).
        // The arg-binding path looks the name up in the root scope
        // directly without any captured-env walk.
        Expr::Variable(_) => true,
        // Cross-scope reference (`&sibling.foo`); the tree-walker's
        // scope chain handles it in O(depth) which is constant for a
        // root-level body. Still trivial — no JIT codegen for
        // reference resolution to amortise.
        Expr::Reference { .. } => true,
        // Recurse into the arithmetic sub-tree. Binary / unary over
        // trivial leaves stays trivial.
        Expr::Binary(op, lhs, rhs) => {
            // Logical short-circuit ops `And` / `Or` route through the
            // tree-walker's closure-style evaluation in some shapes;
            // restrict to pure arithmetic + comparison + concat to
            // stay on the well-trodden constant-time leg.
            if !is_trivial_op(op) {
                return false;
            }
            is_trivial_body(&lhs.expr) && is_trivial_body(&rhs.expr)
        }
        Expr::Unary(op, inner) => is_trivial_op(op) && is_trivial_body(&inner.expr),
        // Ternary keeps three trivial sub-nodes.
        Expr::Ternary { cond, then, els } => {
            is_trivial_body(&cond.expr) && is_trivial_body(&then.expr) && is_trivial_body(&els.expr)
        }
        // Everything else (closures, comprehensions, fn calls, list
        // / dict literals, matches, where bindings, f-strings,
        // variant ctors, spread, wildcard, type expressions) is
        // disqualified.
        _ => false,
    }
}

/// Whitelist of operators that stay on the constant-cost tree-walker
/// path. We exclude `Pipe` (calls into stdlib / user closures) and
/// `Concat` for now — concat over Strings is constant-cost but the
/// W11 envelope is dominated by `+` / `-` / `*` arithmetic and
/// keeping the set tight reduces classifier false-positives. Future
/// extension can opt back in once we have a regression target.
fn is_trivial_op(op: &Operator) -> bool {
    matches!(
        op,
        Operator::Add
            | Operator::Sub
            | Operator::Mul
            | Operator::Div
            | Operator::Mod
            | Operator::Eq
            | Operator::Ne
            | Operator::Lt
            | Operator::Gt
            | Operator::Le
            | Operator::Ge
            | Operator::Not
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_evaluator_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AutoEvaluator>();
    }

    #[test]
    fn trivial_classifier_accepts_w11_shape() {
        // The exact source the W11 bench uses. The classifier must
        // accept it — that's the entire point of the default-path
        // optimisation.
        assert!(is_trivial_scalar_main("#main(Int x) -> Int\nx + 1"));
    }

    #[test]
    fn trivial_classifier_accepts_multi_scalar_params() {
        // Multi-scalar `#main` with arith body still trivial.
        assert!(is_trivial_scalar_main(
            "#main(Int x, Int y) -> Int\nx * y + 7"
        ));
    }

    #[test]
    fn trivial_classifier_rejects_list_param() {
        // `List<Int>` is a generic and not a scalar — must reject.
        assert!(!is_trivial_scalar_main("#main(List<Int> xs) -> Int\n0"));
    }

    #[test]
    fn trivial_classifier_rejects_closure_body() {
        // Body wraps a closure, which the tree-walker can run but
        // would not be a stable / negligible-cost equivalent of
        // the AOT path. Reject.
        assert!(!is_trivial_scalar_main(
            "#main(Int x) -> Int\n((Int n) => n * 2)(x)"
        ));
    }

    #[test]
    fn trivial_classifier_rejects_library_mode() {
        // No `#main(...)` at all — library mode. The classifier
        // returns false so `run_main` would also surface a
        // tree-walker-side `NoMainSignature` shape rather than the
        // AOT branch's `Unsupported`.
        assert!(!is_trivial_scalar_main("{ host: \"localhost\" }"));
    }

    #[test]
    fn trivial_classifier_rejects_import_bearing_source() {
        // `#import` on the entry means there's a transitive module
        // graph; the trivial body might still type-check, but the
        // tree-walker's lazy module load needs the workspace
        // analyzer's BFS to have completed. Conservative reject.
        assert!(!is_trivial_scalar_main(
            "#import std from \"std/string\"\n#main(Int x) -> Int\nx + 1"
        ));
    }

    #[test]
    fn trivial_classifier_rejects_fn_call_body() {
        // Body invokes a fn call (here a stdlib reference). Even
        // though the args are trivial leaves, the call itself routes
        // through native fn dispatch — keep it on the AOT route so
        // the tree-walker's scalar-arith fast path stays narrow.
        assert!(!is_trivial_scalar_main("#main(Int x) -> Int\nabs(x)"));
    }
}
