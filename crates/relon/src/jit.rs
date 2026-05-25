//! Dart-style canonical JIT entry — [`JitEvaluator`].
//!
//! Pairs with [`relon_codegen_native::AotEvaluator`] to expose a
//! two-mode user-facing surface (JIT vs AOT) over the three internal
//! tiers Relon already ships:
//!
//! * **`JitTier::TreeWalk`** — initial interpretation + fallback for
//!   the four non-`run_main` `Evaluator` methods. Always present
//!   because every other tier (bytecode VM, trace JIT) can only
//!   answer `run_main`, and `eval` / `eval_root` / `force_thunk` /
//!   `invoke_closure` need an AST-aware backend.
//! * **`JitTier::Bytecode`** — the M2-A scalar-envelope stack VM
//!   (`relon_bytecode::BytecodeEvaluator`). Populated lazily on the
//!   first `run_main` if `BytecodeEvaluator::from_source` accepts the
//!   shape; rejected sources transparently fall through to the
//!   tree-walker so the dispatcher never panics on out-of-envelope
//!   workloads.
//! * **`JitTier::Trace`** — the cranelift-emitted hot-trace JIT. Not a
//!   standalone evaluator: the trace recorder / installer hooks into
//!   the bytecode evaluator's hot-counter prologue (see
//!   [`relon_bytecode::BytecodeEvaluator::with_hot_trigger`] and
//!   [`relon_codegen_native::trace_install`]). Once a host wires those
//!   hooks against this wrapper's bytecode tier, the trace install +
//!   dispatch flips automatically without any change to
//!   [`JitEvaluator::run_main`]. The enum variant exists today so the
//!   public surface matches the design doc; v1 of the wrapper does
//!   **not** drive its own hot-counter — that's the
//!   tier-transition follow-up.
//!
//! ## Why a thin wrapper and not auto-tier escalation?
//!
//! Per the naming-alignment design note, v1 of the Dart-style JIT
//! split is **purely a naming + organisation refactor**. The four
//! existing evaluator constructions stay; this struct just collects
//! them under one type so hosts see a single `JitEvaluator` instead
//! of choosing between `TreeWalkEvaluator` / `BytecodeEvaluator` /
//! the dispatcher-hook-wired trace JIT. Counter-driven tier escalation
//! inside `run_main` is a follow-up — landing it here today would
//! force a re-design of the existing trace-install path (which already
//! lives at the bytecode layer) and risks regressing the hot-loop
//! benches that pinned a specific bytecode-with-trigger configuration.
//!
//! Hosts that want the auto-tier flavour pair this wrapper with
//! [`crate::Backend::Auto`] / [`crate::AutoEvaluator`], which already
//! routes `run_main` through cranelift-AOT lazily.

use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_parser::Node;

use crate::BackendError;

/// Internal tier classification surfaced via [`JitEvaluator::active_tier`].
/// Mirrors the design-doc taxonomy so observability / test hooks can
/// assert the dispatcher chose the expected backend without poking at
/// concrete evaluator types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitTier {
    /// Tree-walking interpreter. Initial tier; also the fallback for
    /// the four `Evaluator` methods that aren't `run_main` (`eval` /
    /// `eval_root` / `force_thunk` / `invoke_closure`).
    TreeWalk,
    /// Stack-based bytecode VM. Selected when the source survives
    /// `BytecodeEvaluator::from_source`'s M2-A scalar envelope check.
    Bytecode,
    /// Cranelift-emitted hot-trace JIT. Reserved variant; the
    /// trace-install hooks attach at the bytecode tier so the wrapper
    /// transparently picks up trace dispatch once a host wires the
    /// hot-counter trigger. v1 of the wrapper never reports this tier
    /// directly — it surfaces as `Bytecode` from `active_tier` because
    /// the bytecode evaluator owns the trace dispatch handle.
    Trace,
}

/// Dart-style canonical JIT entry. Wraps the tree-walker (always
/// present) plus an optional bytecode VM (populated when the source
/// survives the M2-A envelope check). The trace JIT layer attaches to
/// the bytecode tier through the hot-counter trigger / trace-install
/// hooks in `relon-codegen-native::trace_install` — no separate
/// evaluator instance to wire.
///
/// Construct via [`JitEvaluator::new`] or
/// [`crate::new_evaluator`] with [`crate::Backend::Jit`]. The wrapper
/// stores the source so future tier-promotion work (counter-driven
/// trace install, e.g.) has the bytes it needs without re-asking the
/// caller.
pub struct JitEvaluator {
    /// Tree-walking interpreter — always live. Boxed so the wrapper
    /// stays `Send + Sync` without bleeding `TreeWalkEvaluator`'s
    /// generics outward.
    tree_walk: Box<TreeWalkEvaluator>,
    /// Optional bytecode-VM tier. `None` when the source falls outside
    /// the M2-A envelope (closures / list / dict / stdlib) or when
    /// the bytecode setup raised a non-envelope error; either way the
    /// wrapper transparently routes `run_main` back through the
    /// tree-walker.
    bytecode: Option<Box<dyn Evaluator>>,
}

impl JitEvaluator {
    /// Build a [`JitEvaluator`] over `source`. The tree-walker tier is
    /// constructed eagerly (cheap, ~1 ms). The bytecode tier is also
    /// built eagerly today — `BytecodeEvaluator::from_source` runs the
    /// same parse / analyse / lower pipeline the tree-walker already
    /// drove, so a separate lazy slot would just add bookkeeping
    /// without saving cold-start cycles for the hosts the auto-tier
    /// path optimises for. Sources outside the M2-A envelope skip the
    /// bytecode build entirely and leave the slot at `None`.
    pub fn new(source: &str) -> std::result::Result<Self, BackendError> {
        let node = relon_parser::parse_document(source)
            .map_err(|e| BackendError::Parse(e.to_string()))?;
        let tree_walk = crate::build_tree_walk_evaluator_from_parsed(node)?;
        let bytecode = match relon_bytecode::BytecodeEvaluator::from_source(source) {
            Ok(ev) => Some(Box::new(ev) as Box<dyn Evaluator>),
            Err(_envelope_reject) => {
                // M2-A scalar-envelope rejections are expected for any
                // workload touching list / dict / stdlib / closure.
                // Surfacing this as a setup error would force every
                // such source onto the `Backend::TreeWalk` path; the
                // wrapper instead silently falls back to the
                // tree-walker, matching the "user picks JIT, the
                // dispatcher picks the tier" contract.
                None
            }
        };
        Ok(Self {
            tree_walk: Box::new(tree_walk),
            bytecode,
        })
    }

    /// Returns the tier the dispatcher would currently route a
    /// `run_main` call through. Used by tests / observability hooks.
    /// Today this is either [`JitTier::Bytecode`] (when the bytecode
    /// tier was successfully built) or [`JitTier::TreeWalk`].
    /// [`JitTier::Trace`] is reserved for a future revision that
    /// drives the hot-counter promotion explicitly from inside the
    /// wrapper rather than implicitly through the bytecode tier's
    /// own dispatcher.
    pub fn active_tier(&self) -> JitTier {
        if self.bytecode.is_some() {
            JitTier::Bytecode
        } else {
            JitTier::TreeWalk
        }
    }

    /// Whether the bytecode tier survived setup. Mirrors
    /// [`Self::active_tier`] for the common boolean question hosts ask
    /// in smoke tests.
    pub fn has_bytecode_tier(&self) -> bool {
        self.bytecode.is_some()
    }
}

impl Evaluator for JitEvaluator {
    fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Only the tree-walker exposes arbitrary-node evaluation; the
        // bytecode and trace tiers are `run_main`-only.
        self.tree_walk.eval(node, scope)
    }

    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Library / static-config path; tree-walker always.
        self.tree_walk.eval_root(scope)
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // Dispatch order: bytecode tier first (it's the trace-install
        // landing pad as well, so a wired trace JIT picks up
        // transparently), tree-walker fallback when the bytecode setup
        // rejected the source's shape.
        if let Some(bc) = &self.bytecode {
            match bc.run_main(args.clone()) {
                Ok(v) => return Ok(v),
                Err(RuntimeError::Unsupported { .. }) => {
                    // The bytecode tier surfaced an envelope-edge op
                    // it can't execute (M2-A leaves several ops as
                    // `Unsupported`). Quietly fall through to the
                    // tree-walker so the host still gets an answer.
                    tracing::debug!(
                        target: "relon::jit_evaluator",
                        "bytecode tier returned Unsupported; falling back to tree-walker"
                    );
                }
                Err(other) => return Err(other),
            }
        }
        Evaluator::run_main(self.tree_walk.as_ref(), args)
    }

    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        self.tree_walk.force_thunk(thunk)
    }

    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError> {
        self.tree_walk.invoke_closure(closure, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial scalar `#main` survives the bytecode envelope: the
    /// dispatcher should report `Bytecode` as the active tier and
    /// `run_main` must return the same value the tree-walker would.
    #[test]
    fn scalar_main_routes_through_bytecode_tier() {
        let src = "#main(Int x) -> Int\nx + 1";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert_eq!(jit.active_tier(), JitTier::Bytecode);
        assert!(jit.has_bytecode_tier());

        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(41));
        let out = jit.run_main(args).expect("run_main");
        assert_eq!(out, Value::Int(42));
    }

    /// Non-scalar shapes (list literal body here) fall outside the
    /// M2-A envelope. The wrapper must skip the bytecode build,
    /// report `TreeWalk` as the active tier, and still answer
    /// `run_main` correctly via the tree-walker fallback.
    #[test]
    fn non_scalar_main_falls_back_to_tree_walk() {
        let src = "#main(Int n) -> List<Int>\n[n, n + 1, n + 2]";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert_eq!(jit.active_tier(), JitTier::TreeWalk);
        assert!(!jit.has_bytecode_tier());

        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(10));
        let out = jit.run_main(args).expect("run_main");
        match out {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Int(10));
                assert_eq!(items[1], Value::Int(11));
                assert_eq!(items[2], Value::Int(12));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    /// `eval_root` / `force_thunk` / `invoke_closure` always go
    /// through the tree-walker. Sanity check: a no-`#main` library
    /// document evaluates the same as via [`crate::value_from_str`].
    #[test]
    fn library_mode_works_via_eval_root() {
        let src = r#"{ host: "x", port: 80 }"#;
        let jit = JitEvaluator::new(src).expect("build jit");
        let scope = Arc::new(Scope::default());
        let value = jit.eval_root(&scope).expect("eval_root");
        match value {
            Value::Dict(d) => {
                let host = d.map.get("host").expect("host");
                assert_eq!(host, &Value::String("x".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }
}
