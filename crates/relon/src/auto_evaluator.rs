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
//!   `CraneliftAotEvaluator` so future backends can swap in without
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
use relon_parser::Node;

use crate::{build_tree_walk_evaluator, BackendError};

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
    aot_init_err: OnceLock<String>,
}

impl AutoEvaluator {
    /// Build an [`AutoEvaluator`] over `source`. The tree-walker is
    /// constructed eagerly — same pipeline `Backend::TreeWalk`
    /// uses — so the wrapper is immediately ready to serve `eval` /
    /// `eval_root` / `force_thunk` / `invoke_closure`. The AOT
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

    /// Drive the AOT pipeline over `source`.
    ///
    /// v5-β-2 stage 4: cranelift-AOT is the only AOT backend left.
    /// When the `cranelift-aot` feature is enabled this returns the
    /// boxed evaluator; otherwise the slot caches a "feature off"
    /// error and `run_main` returns `RuntimeError::Unsupported`.
    #[cfg(feature = "cranelift-aot")]
    fn build_aot(source: &str) -> Result<Box<dyn Evaluator>, String> {
        relon_codegen_native::CraneliftAotEvaluator::from_source(source)
            .map(|aot| Box::new(aot) as Box<dyn Evaluator>)
            .map_err(|e| e.to_string())
    }

    /// Stub for builds compiled without `cranelift-aot` (e.g. wasm32
    /// hosts). `run_main` surfaces the cached error; the tree-walker
    /// surface keeps working.
    #[cfg(not(feature = "cranelift-aot"))]
    fn build_aot(_source: &str) -> Result<Box<dyn Evaluator>, String> {
        Err("this build was compiled without the `cranelift-aot` feature; rebuild with `--features cranelift-aot` to enable the AOT backend"
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
                reason: format!("auto backend: cranelift-AOT setup failed: {msg}"),
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
