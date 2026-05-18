//! Auto-tier evaluator wrapper.
//!
//! v4-e landing surface for `Backend::Auto`. An [`AutoEvaluator`]
//! eagerly constructs a [`TreeWalkEvaluator`] (cheap, ~1 ms) and
//! lazily initialises the wasm-AOT backend only when a host first
//! calls [`relon_eval_api::Evaluator::run_main`]. The other four
//! `Evaluator` methods (`eval`, `eval_root`, `force_thunk`,
//! `invoke_closure`) always go through the tree-walker — which is
//! also the only backend that supports them today.
//!
//! ## Design notes
//!
//! * **Lazy AOT init** keeps the wasted-work scenario (host parses,
//!   reads static config via `eval_root`, never calls `run_main`)
//!   free of any wasm-AOT compile / wasmtime cold-start cost. The
//!   benchmark numbers in `docs/internal/wasm-bench-report-...`
//!   appendix A.21 show the AOT setup runs ~2 ms uncached / ~190 μs
//!   cached on the current shared engine.
//! * **Thread-safe + cache-once.** Both the success and failure
//!   paths are cached in `OnceLock`s, so concurrent `run_main`
//!   callers only ever drive the AOT pipeline once.
//! * **AOT setup failure isolation.** If the wasm-AOT pipeline fails
//!   (build compiled without `wasm-aot`, source uses constructs the
//!   AOT backend rejects, ...), only `run_main` returns an error;
//!   `eval` / `eval_root` / `force_thunk` / `invoke_closure` keep
//!   working off the tree-walker.
//! * **`Box<dyn Evaluator>`** is stored rather than the concrete
//!   `WasmAotEvaluator` so v5-β can swap the cranelift-AOT backend
//!   in without changing this file's surface.
//!
//! v5-β note: the body of [`AutoEvaluator::build_aot`] is the only
//! v4-e site that hard-codes the wasm-AOT backend. Swapping it for a
//! cranelift-AOT call is a localized change; the wrapper structure
//! and the public `Backend::Auto` API stay frozen.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_parser::Node;

use crate::{build_tree_walk_evaluator, BackendError};

/// Wrapper [`Evaluator`] that routes the four AST-aware methods
/// through a tree-walker and lazily spins up the wasm-AOT backend on
/// first `run_main`. Constructed via [`AutoEvaluator::new`] or
/// [`crate::new_evaluator`] with [`crate::Backend::Auto`].
pub struct AutoEvaluator {
    /// Source kept around so we can drive the AOT pipeline on first
    /// `run_main`. Owned rather than borrowed so the wrapper outlives
    /// the caller's source string.
    source: String,
    /// Eagerly-constructed tree-walker. Owned via a `Box` so the
    /// wrapper stays `Send + Sync` without bounding `TreeWalkEvaluator`
    /// generics into this file.
    tree_walk: Box<TreeWalkEvaluator>,
    /// Lazy wasm-AOT backend; once the first `run_main` runs the
    /// codegen pipeline successfully, every subsequent call reuses
    /// this slot. Stored as `Box<dyn Evaluator>` so v5-β can swap the
    /// concrete backend without touching this struct's layout.
    aot: OnceLock<Box<dyn Evaluator>>,
    /// Mirror slot for the failure path. If the AOT pipeline fails
    /// once we cache the error so repeated `run_main` calls don't
    /// each pay a fresh parse / analyze / lower / codegen attempt.
    aot_init_err: OnceLock<String>,
}

impl AutoEvaluator {
    /// Build an [`AutoEvaluator`] over `source`. The tree-walker is
    /// constructed eagerly — same pipeline `Backend::TreeWalk`
    /// uses — so the wrapper is immediately ready to serve `eval` /
    /// `eval_root` / `force_thunk` / `invoke_closure`. The wasm-AOT
    /// half stays unbuilt until the first `run_main` invocation.
    pub fn new(source: &str) -> std::result::Result<Self, BackendError> {
        let tree_walk = build_tree_walk_evaluator(source)?;
        Ok(Self {
            source: source.to_string(),
            tree_walk: Box::new(tree_walk),
            aot: OnceLock::new(),
            aot_init_err: OnceLock::new(),
        })
    }

    /// Returns `true` if the wasm-AOT backend has been constructed
    /// (either successfully or with a cached error). Exposed for
    /// tests / observability so a host can assert that lazy init
    /// actually stayed lazy across an `eval` / `eval_root` path.
    pub fn is_aot_initialised(&self) -> bool {
        self.aot.get().is_some() || self.aot_init_err.get().is_some()
    }

    /// Reach the lazy AOT backend, building it on demand. Returns
    /// the cached error reference if a prior call already failed —
    /// the AOT pipeline is deterministic, so retrying after a failure
    /// would just burn CPU on the same error.
    fn aot(&self) -> std::result::Result<&dyn Evaluator, &str> {
        // Fast path: already built successfully.
        if let Some(aot) = self.aot.get() {
            return Ok(aot.as_ref());
        }
        // Fast path: already failed; surface cached error without
        // re-running the pipeline.
        if let Some(err) = self.aot_init_err.get() {
            return Err(err.as_str());
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
            Err(msg) => {
                let _ = self.aot_init_err.set(msg);
                Err(self
                    .aot_init_err
                    .get()
                    .expect("aot_init_err slot populated after set / race-loss")
                    .as_str())
            }
        }
    }

    /// Drive the wasm-AOT pipeline over `source`. Kept as an
    /// associated fn so the v5-β rewire only touches one site.
    #[cfg(feature = "wasm-aot")]
    fn build_aot(source: &str) -> Result<Box<dyn Evaluator>, String> {
        relon_codegen_wasm::WasmAotEvaluator::from_source(source)
            .map(|aot| Box::new(aot) as Box<dyn Evaluator>)
            .map_err(|e| e.to_string())
    }

    /// Stub fallback for builds compiled without the `wasm-aot`
    /// feature (e.g. the `wasm32-unknown-unknown` target). The
    /// tree-walker surface stays usable; only `run_main` will
    /// surface this error.
    #[cfg(not(feature = "wasm-aot"))]
    fn build_aot(_source: &str) -> Result<Box<dyn Evaluator>, String> {
        Err("this build was compiled without the `wasm-aot` feature; rebuild with `--features wasm-aot` to enable AOT run_main"
            .to_string())
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
        match self.aot() {
            Ok(aot) => aot.run_main(args),
            Err(msg) => Err(RuntimeError::Unsupported {
                reason: format!("auto backend: wasm-AOT setup failed: {msg}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_evaluator_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AutoEvaluator>();
    }
}
