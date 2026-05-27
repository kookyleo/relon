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
use wasmtime::{Config, Engine, Instance, Linker, Module, Store, TypedFunc};

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
    #[allow(dead_code)]
    instance: Instance,
    program: WasmProgram,
    /// Phase Z.3a: cache the resolved `TypedFunc<i64, i64>` for
    /// `__main`. `Instance::get_typed_func` does signature lookup +
    /// validation against the wasmtime export table (string compare on
    /// the symbol name + funcref unwrap + `WasmTy::matches` walk).
    /// That cost is ~100-200 ns per call when re-resolved inside a
    /// hot loop. The Z.1 programs (W1/W6/W12) all share the
    /// `(i64) -> i64` signature, so we resolve once at construction
    /// time and reuse the typed handle for both the slow (`run_main`)
    /// and fast (`run_main_legacy_i64_fast`) entries.
    main_typed: TypedFunc<i64, i64>,
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

        // Phase Z.3a: resolve `__main` as `TypedFunc<i64, i64>` once
        // and cache it. All Z.1 programs (W1/W6/W12) match this
        // signature; a future Z.3 widening that adds Float/String
        // returns will need a per-program typed handle variant.
        let main_typed = instance
            .get_typed_func::<i64, i64>(&mut store, "__main")
            .map_err(|e| WasmEvalError::Wasmtime(format!("get __main typed func: {e}")))?;

        Ok(Self {
            tree_walk: Box::new(tree_walk),
            source: source.to_string(),
            wasm: Some(WasmRuntime {
                store: Mutex::new(store),
                instance,
                program,
                main_typed,
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

    /// Phase Z.3a: is the cached `(i64) -> i64` fast-entry handle
    /// available **and** semantically meaningful for this evaluator?
    /// Returns `true` when the source classified into a Z.1/Z.3
    /// program whose i64 return value **is** the scalar Int result
    /// the host would wrap in `Value::Int` (W1/W2/W6/W10-inline/W12);
    /// `false` when:
    ///
    /// - the source fell through to the tree-walker tier (`wasm: None`), or
    /// - the i64 return encodes a non-Int payload — e.g. W3 inline,
    ///   where the i64 is the packed `(ptr<<32 | len)` of a String
    ///   that needs a linear-memory copy before it can become a
    ///   `Value::String`. The fast path bypasses that copy, so
    ///   surfacing it as available would silently corrupt the row's
    ///   measurement (it would book a meaningless raw i64 timing
    ///   under the `wasmtime_fast` label).
    ///
    /// The cmp_lua bench's `relon_wasm_wasmtime_fast` row gates on
    /// this so a scope-cut source never books fast-path numbers —
    /// matches the `LlvmAotEvaluator::has_fast_path` contract.
    pub fn has_fast_path(&self) -> bool {
        match &self.wasm {
            None => false,
            Some(rt) => program_returns_scalar_int(rt.program),
        }
    }

    /// Phase Z.3a dispatch-boundary fast path: invoke the cached
    /// `(i64) -> i64` `__main` typed-func directly with the supplied
    /// positional `i64` arg. Bypasses the `HashMap<String, Value>`
    /// pack + per-arg `extract_named_int` walk + the
    /// `Value::Int(out)` wrap on the return.
    ///
    /// The remaining boundary cost on this path is:
    ///   - one `Mutex::lock` on the store (uncontested in steady-
    ///     state — single-threaded driver)
    ///   - one `HostState::reset` (arena cursor write)
    ///   - one `TypedFunc::call` (~150-250 ns on x86_64; this is the
    ///     wasmtime ABI floor, see comment on `main_typed`)
    ///   - `mark_compiled` / `mark_deopt` tier write
    ///
    /// Returns `Err(Unsupported)` when the source fell through to
    /// the tree-walker tier (`wasm: None`).
    pub fn run_main_legacy_i64_fast(&self, args: &[i64]) -> Result<i64, RuntimeError> {
        let rt = self
            .wasm
            .as_ref()
            .ok_or_else(|| RuntimeError::Unsupported {
                reason: "wasm-eval: fast path unavailable (source on tree-walker fallback)".into(),
            })?;
        if !program_returns_scalar_int(rt.program) {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "wasm-eval fast path: program {:?} returns a non-Int payload \
                     (e.g. packed ptr/len); use run_main",
                    rt.program
                ),
            });
        }
        if args.len() != 1 {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "wasm-eval fast path: Z.1 programs take 1 arg, got {}",
                    args.len()
                ),
            });
        }
        let arg = args[0];
        let mut store = rt
            .store
            .lock()
            .map_err(|_| io_err("wasm-eval store mutex poisoned"))?;
        store.data_mut().reset();
        match rt.main_typed.call(&mut *store, arg) {
            Ok(out) => {
                store.data_mut().mark_compiled();
                Ok(out)
            }
            Err(e) => {
                store.data_mut().mark_deopt();
                Err(io_err(format!("wasm-eval: __main trapped: {e}")))
            }
        }
    }
}

/// Does the program's `(i64) -> i64` typed-func return value carry a
/// scalar Int (as opposed to a packed `(ptr<<32 | len)` String handle
/// or some other Z.4 return shape)? Drives both `has_fast_path()` and
/// the fast-path entry's eligibility check.
fn program_returns_scalar_int(program: WasmProgram) -> bool {
    match program {
        WasmProgram::W1IntSumRange
        | WasmProgram::W2DotProduct
        | WasmProgram::W6ListSumPlusOne
        | WasmProgram::W10ConfigEvalInline
        | WasmProgram::W12IncrementInt => true,
        WasmProgram::W3StringConcatInline => false,
        // Scope-cut variants never instantiate a wasm runtime, so this
        // arm is unreachable in practice — but a hard match keeps the
        // check exhaustive so adding a future return shape forces a
        // conscious decision.
        WasmProgram::W3StringConcat
        | WasmProgram::W4StringContains { .. }
        | WasmProgram::W5DictAccess
        | WasmProgram::W7FibRecursion
        | WasmProgram::W8PolymorphicDispatch
        | WasmProgram::W9NestedMatrix
        | WasmProgram::W10ConfigEval => false,
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

        // Pack args. All Z.1/Z.3 programs declare a single `#main(Int)`
        // parameter; pick the canonical arg name based on the variant.
        let arg_i64 = match rt.program {
            WasmProgram::W1IntSumRange
            | WasmProgram::W2DotProduct
            | WasmProgram::W3StringConcatInline
            | WasmProgram::W6ListSumPlusOne
            | WasmProgram::W10ConfigEvalInline => extract_named_int(&args, "n")?,
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
        // Phase Z.3a: typed-func is cached on `rt.main_typed`, no
        // per-call `get_typed_func` resolve. The buffer-protocol
        // path (this method) carries the HashMap pack + the per-
        // variant return-shape unpack (Int wrap for the scalar
        // returns, ptr/len -> String copy for W3 inline).
        let out = match rt.main_typed.call(&mut *store, arg_i64) {
            Ok(out) => {
                store.data_mut().mark_compiled();
                out
            }
            Err(e) => {
                store.data_mut().mark_deopt();
                return Err(io_err(format!("wasm-eval: __main trapped: {e}")));
            }
        };
        match rt.program {
            WasmProgram::W3StringConcatInline => {
                // Unpack the (ptr<<32 | len) i64 produced by
                // `emit_w3_string_concat_inline` and copy the bytes
                // back into a `String`. The bytes live in the per-
                // call arena slice and will be overwritten by the
                // next `HostState::reset`; copy now while the slice
                // is still valid.
                let packed = out as u64;
                let ptr = (packed >> 32) as u32;
                let len = (packed & 0xffff_ffff) as u32;
                let memory = rt
                    .instance
                    .get_memory(&mut *store, "memory")
                    .ok_or_else(|| io_err("wasm-eval: instance missing `memory` export"))?;
                let view = memory.data(&*store);
                let start = ptr as usize;
                let end = start.checked_add(len as usize).ok_or_else(|| {
                    io_err(format!(
                        "wasm-eval W3: ptr+len overflow (ptr={ptr}, len={len})"
                    ))
                })?;
                if end > view.len() {
                    return Err(io_err(format!(
                        "wasm-eval W3: ptr+len out of bounds (ptr={ptr}, len={len}, mem={})",
                        view.len()
                    )));
                }
                let bytes = &view[start..end];
                let s = std::str::from_utf8(bytes)
                    .map_err(|e| io_err(format!("wasm-eval W3: invalid UTF-8: {e}")))?
                    .to_string();
                Ok(Value::String(s.into()))
            }
            _ => Ok(Value::Int(out)),
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
