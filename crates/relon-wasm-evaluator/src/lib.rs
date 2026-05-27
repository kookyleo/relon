//! Phase Z runtime: drive a `relon-codegen-wasm`-emitted module through
//! wasmtime, exposing the public `Evaluator` trait.
//!
//! See `docs/internal/phase-z-design.md` §6 for the wasmtime integration
//! design + §7 for the cmp_lua honesty contract.

#![deny(unsafe_code)]
#![deny(missing_docs)]

mod classify;
mod host_imports;
mod host_state;

pub use classify::{classify_main, ClassifyError};
pub use host_state::HostState;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::Node;
use relon_parser::TokenRange;
use wasmtime::{Config, Engine, Instance, Linker, Module, Store};

use relon_codegen_wasm::{lower, WasmProgram};

/// Phase Z evaluator. Each instance owns one compiled `Module` + a
/// reusable `Store<HostState>`. `run_main` resets the arena between
/// calls and dispatches the program's `__main` export.
///
/// Cold-start cost: one parse + classify + emit + `Module::new`. Steady-
/// state: one host-state reset + one wasmtime call.
pub struct WasmEvaluator {
    /// Tree-walker tier for any of the five Evaluator entry points
    /// other than `run_main`. Z.1 only wires `run_main`; the rest
    /// delegate to the reference impl so a misclassified entry doesn't
    /// silently fall through.
    tree_walk: Box<relon_evaluator::TreeWalkEvaluator>,
    /// Source text — kept so error messages can echo back the program.
    /// Cheap because the tree-walker also pins it via `Scope`.
    #[allow(dead_code)]
    source: String,
    /// The wasm module + classifier output. `None` when `classify_main`
    /// returned a `ScopeCut` — in that case `run_main` falls back to
    /// the tree-walker tier, surfacing the scope-cut as a visible
    /// "this row is on the Z.3 roadmap" signal rather than a fake pass.
    wasm: Option<WasmRuntime>,
}

struct WasmRuntime {
    /// Lock-protected so the trait-object `&self`-shaped `run_main`
    /// can still call into wasmtime, which needs `&mut Store`.
    store: Mutex<Store<HostState>>,
    instance: Instance,
    program: WasmProgram,
}

/// Public construction errors. Distinct from `LowerError` because the
/// host wants to distinguish "no `#main`" / "parse failure" /
/// "wasmtime engine init failure" from "lowering scope-cut".
#[derive(Debug, thiserror::Error)]
pub enum WasmEvalError {
    /// Parser couldn't read the source.
    #[error("parse error: {0}")]
    Parse(String),
    /// Classifier didn't recognise the AST shape.
    #[error("classify error: {0}")]
    Classify(ClassifyError),
    /// `relon-codegen-wasm` emit failed.
    #[error("lowering error: {0}")]
    Lower(relon_codegen_wasm::LowerError),
    /// wasmtime `Engine::new` / `Module::new` / `Linker::instantiate`
    /// failure.
    #[error("wasmtime: {0}")]
    Wasmtime(String),
    /// Tree-walker tier construction failure (caller bug — the source
    /// already passed parse).
    #[error("tree-walker tier setup failed: {0}")]
    TreeWalker(String),
}

impl WasmEvaluator {
    /// Build a `WasmEvaluator` from Relon source.
    ///
    /// The tree-walker tier is always constructed (cheap, ~1 ms) — it
    /// covers `eval`, `eval_root`, `force_thunk`, `invoke_closure` plus
    /// any `run_main` whose source the classifier can't lower.
    pub fn new(source: &str) -> Result<Self, WasmEvalError> {
        let node = relon_parser::parse_document(source)
            .map_err(|e| WasmEvalError::Parse(e.to_string()))?;
        let analyzed = Arc::new(relon_analyzer::analyze(&node));

        let mut ctx = relon_evaluator::Context::new()
            .with_root(node.clone())
            .with_analyzed(Arc::clone(&analyzed));
        relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        let tree_walk = relon_evaluator::TreeWalkEvaluator::new(Arc::new(ctx));

        // Try to classify the entry into a Z.1 program. A `ScopeCut`
        // here is fine — we keep the tree-walker tier and route
        // `run_main` through it.
        let program = match classify::classify_main(source) {
            Ok(p) => p,
            Err(ClassifyError::ScopeCut(tag)) => {
                tracing::debug!(
                    target: "relon::wasm_evaluator",
                    workload = tag,
                    "source outside Z.1 lowering surface; routing run_main through tree-walker"
                );
                return Ok(Self {
                    tree_walk: Box::new(tree_walk),
                    source: source.to_string(),
                    wasm: None,
                });
            }
            Err(other) => return Err(WasmEvalError::Classify(other)),
        };

        let bytes = lower(&program).map_err(WasmEvalError::Lower)?;

        let mut config = Config::new();
        config.wasm_tail_call(true);
        let engine = Engine::new(&config).map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;
        let module =
            Module::new(&engine, &bytes).map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;

        let mut linker = Linker::<HostState>::new(&engine);
        host_imports::register_host_imports(&mut linker)
            .map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;

        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;
        // Wire the memory pointer back into the host state so host imports
        // that copy into linear memory know where to write.
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmEvalError::Wasmtime("module missing exported `memory`".into()))?;
        store.data_mut().bind_memory(memory);

        Ok(Self {
            tree_walk: Box::new(tree_walk),
            source: source.to_string(),
            wasm: Some(WasmRuntime {
                store: Mutex::new(store),
                instance,
                program,
            }),
        })
    }

    /// Snapshot of the current tier. Returns `Cold` before any call,
    /// `Compiled` after a successful invoke, or `Deoptimised` after
    /// a host-trap.
    pub fn active_tier(&self) -> Tier {
        match &self.wasm {
            None => Tier::TreeWalker,
            Some(rt) => {
                let store = rt.store.lock().expect("WasmEvaluator store mutex poisoned");
                store.data().tier()
            }
        }
    }
}

/// Public tier label — see design §6.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Source classified out of Z.1 scope; `run_main` routes through
    /// the tree-walker tier.
    TreeWalker,
    /// `Module::new` succeeded, no calls yet.
    Cold,
    /// Last `run_main` returned `Ok(_)` without trapping.
    Compiled,
    /// Last `run_main` raised a wasmtime trap.
    Deoptimised,
}

impl Evaluator for WasmEvaluator {
    fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        self.tree_walk.eval(node, scope)
    }

    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        self.tree_walk.eval_root(scope)
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        let Some(rt) = &self.wasm else {
            // Scope-cut path: tree-walker tier owns the call.
            // The Evaluator trait method on TreeWalkEvaluator takes a
            // single `args` arg (internal default scope); call through
            // the trait so the override stays uniform.
            return Evaluator::run_main(self.tree_walk.as_ref(), args);
        };

        // Pack args. Z.1 programs are all `#main(Int n) -> Int` or
        // `#main(Int x) -> Int`, so we look up the single declared
        // parameter and route it as `i64`.
        let arg_i64 = match rt.program {
            WasmProgram::W1IntSumRange | WasmProgram::W6ListSumPlusOne => {
                extract_named_int(&args, "n")?
            }
            WasmProgram::W12IncrementInt => extract_named_int(&args, "x")?,
            other => {
                return Err(io_err(format!(
                    "wasm-eval: program variant {other:?} reached run_main \
                     but lacks a packing rule (Z.1 bug — should have been ScopeCut)"
                )))
            }
        };

        let mut store = rt
            .store
            .lock()
            .map_err(|_| io_err("wasm-eval store mutex poisoned"))?;
        store.data_mut().reset();
        let main = rt
            .instance
            .get_typed_func::<i64, i64>(&mut *store, "__main")
            .map_err(|e| io_err(format!("wasm-eval: get __main: {e}")))?;
        match main.call(&mut *store, arg_i64) {
            Ok(out) => {
                store.data_mut().mark_compiled();
                Ok(Value::Int(out))
            }
            Err(e) => {
                store.data_mut().mark_deopt();
                Err(io_err(format!("wasm-eval: __main trapped: {e}")))
            }
        }
    }

    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        self.tree_walk.force_thunk(thunk)
    }

    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError> {
        self.tree_walk.invoke_closure(closure, args)
    }
}

fn extract_named_int(args: &HashMap<String, Value>, name: &str) -> Result<i64, RuntimeError> {
    match args.get(name) {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => Err(io_err(format!(
            "wasm-eval: arg `{name}` must be Int, got {other:?}"
        ))),
        None => Err(RuntimeError::MissingMainArg {
            name: name.to_string(),
            range: TokenRange::default(),
        }),
    }
}

/// Wrap an internal wasm-evaluator failure into a `RuntimeError`. The
/// `RuntimeError` taxonomy has no "internal-error" carrier, so we re-use
/// `IoError` (which is the closest existing surface — a runtime-side
/// failure unrelated to the user's source). Z.3 may add a dedicated
/// `WasmInternal` variant if this proves noisy.
fn io_err(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::IoError(msg.into())
}

// Re-exports for crate consumers that only want the surface, not the
// inner module names.
pub use relon_codegen_wasm::LowerError as CodegenError;
