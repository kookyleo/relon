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

use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Schema, TypeRepr};
use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_ir::{MAIN_RETURN_SCHEMA_NAME, RETURN_VALUE_FIELD_NAME};
use relon_parser::Node;
use relon_parser::TokenRange;
use wasmtime::{Config, Engine, Instance, Linker, Module, Store, TypedFunc};

use relon_codegen_wasm::{const_segment_end, lower, lower_ir_module, WasmProgram};

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
    /// Lowering path tag — drives the per-program return-shape unpack
    /// in `run_main`. The classifier path (`Classifier(WasmProgram)`)
    /// uses the existing per-variant handlers; the Z.4.0 IR-walker
    /// path (`IrWalker`) returns a scalar i64 wrapped in
    /// `Value::Int` since the walker only handles scalar-Int return
    /// shapes today.
    program: ProgramSource,
    /// Phase Z.3a: cache the resolved `TypedFunc<i64, i64>` for
    /// `__main`. `Instance::get_typed_func` does signature lookup +
    /// validation against the wasmtime export table (string compare on
    /// the symbol name + funcref unwrap + `WasmTy::matches` walk).
    /// That cost is ~100-200 ns per call when re-resolved inside a
    /// hot loop. The Z.1 programs (W1/W6/W12) all share the
    /// `(i64) -> i64` signature, so we resolve once at construction
    /// time and reuse the typed handle for both the slow (`run_main`)
    /// and fast (`run_main_legacy_i64_fast`) entries. Z.4.0's IR-
    /// walker path requires every `#main(Int...)` parameter to land
    /// on this typed handle's single-i64 input slot — the W1-W12
    /// panel is single-Int-param across the board, so the constraint
    /// holds for the current panel. Multi-param widening is a Z.4
    /// follow-up.
    main_typed: TypedFunc<i64, i64>,
    /// Arg-name the typed-func expects. Classifier path picks per-
    /// variant; IR-walker path reads it off the `MainParams` schema's
    /// first field. Cached so `run_main` doesn't re-resolve.
    arg_name: String,
}

/// Lowering provenance of the active program. Drives the per-program
/// arg-pack + return-unpack discipline in `run_main`. Z.1's POC kept a
/// `WasmProgram` enum value directly on the runtime; Z.4.0 widens it
/// to also recognise the IR-walker path so the host can dispatch
/// uniformly.
#[derive(Debug, Clone)]
enum ProgramSource {
    /// Classifier matched a known cmp_lua workload — the runtime uses
    /// the variant's per-program packing / unpacking rules.
    Classifier(WasmProgram),
    /// Phase Z.4.0+ — IR walker emitted the module. The
    /// [`IrWalkerReturn`] discriminator picks the per-call return-
    /// unpack discipline (scalar Int wrap for Z.4.0; Dict-record
    /// decode for Z.4.1).
    IrWalker(IrWalkerReturn),
}

/// Z.4.0+ — Return-shape provenance for the IR-walker path. The
/// walker's typed-func signature stays `(i64) -> i64` across all
/// variants; the i64's *meaning* differs.
#[derive(Debug, Clone)]
enum IrWalkerReturn {
    /// Z.4.0 — the i64 is a scalar Int value; `run_main` wraps it as
    /// `Value::Int(out)`.
    ScalarInt,
    /// Z.4.1 — the i64 is a zero-extended i32 arena pointer to a
    /// record laid out per `schema` / `layout`. `run_main` walks the
    /// schema to decode each field out of linear memory into a
    /// `Value::Dict`.
    DictRecord {
        /// Synthesised return schema. Owned so the runtime stays
        /// thread-safe without re-borrowing the original lowering
        /// output.
        schema: Schema,
        /// Per-field offset table for `schema`. Re-derived from the
        /// schema once at instantiate time so the per-call decode
        /// avoids the `SchemaLayout::offsets_for` re-walk.
        layout: OffsetTable,
    },
    /// Z.4.2 — the i64 is a zero-extended i32 absolute pointer to a
    /// `List<Int>` record (`[len: u32 LE][pad: u32 zero][i64
    /// elements...]`). The walker installs the record as an active
    /// data segment at module instantiate time; `run_main` reads the
    /// header + payload back out of linear memory and wraps the
    /// elements as a `Value::List`.
    ListInt,
}

/// Bundled wasmtime objects returned from
/// [`WasmEvaluator::build_runtime`]. Pulled into a struct so the
/// public ctor doesn't return a `(Store, Instance, TypedFunc)` tuple
/// that trips clippy's `type_complexity` check.
struct InstantiateOutcome {
    store: Store<HostState>,
    instance: Instance,
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

        // Lowering tier ordering (Z.4.0):
        //
        // 1. **Classifier** — the 12-row cmp_lua panel routes here.
        //    Each variant has a hand-emitted lowering tuned for the
        //    panel's specific bytecode-shape source; first dibs so
        //    no panel row silently drops to a less-optimised path.
        // 2. **IR walker** — Phase Z.4.0's canonical lowering. Runs
        //    `parse + analyze + lower_workspace_single + lower_ir_module`.
        //    Catches sources outside the classifier's pattern envelope
        //    but inside the IR walker's scalar-Int subset (e.g. the
        //    arithmetic combinators not pinned to a cmp_lua row).
        // 3. **Tree-walker fallback** — anything the first two reject.
        let classifier_outcome = classify::classify_main(source);
        let program = match classifier_outcome {
            Ok(p) => Some(p),
            Err(ClassifyError::ScopeCut(_)) => None,
            Err(other) => return Err(WasmEvalError::Classify(other)),
        };

        if let Some(program) = program {
            return Self::instantiate_classifier(source, node.clone(), tree_walk, program);
        }

        // Z.4.0 IR walker path. The walker only handles the scalar-Int
        // subset; non-Int / Dict / List / closure sources will scope-
        // cut and we fall through to the tree-walker tier.
        match try_lower_ir_walker(&node) {
            Ok(walker) => Self::instantiate_walker(source, tree_walk, walker),
            Err(IrWalkerSkipReason::IrLoweringFailed(e)) => {
                tracing::debug!(
                    target: "relon::wasm_evaluator",
                    err = %e,
                    "Z.4.0 IR lowering refused source; routing run_main through tree-walker"
                );
                Ok(Self {
                    tree_walk: Box::new(tree_walk),
                    source: source.to_string(),
                    wasm: None,
                })
            }
            Err(IrWalkerSkipReason::WalkerScopeCut(tag, reason)) => {
                tracing::debug!(
                    target: "relon::wasm_evaluator",
                    op = tag,
                    reason = reason,
                    "Z.4.0 IR walker scope-cut; routing run_main through tree-walker"
                );
                Ok(Self {
                    tree_walk: Box::new(tree_walk),
                    source: source.to_string(),
                    wasm: None,
                })
            }
        }
    }

    /// Common wasmtime instantiation given an emitted wasm module +
    /// optional const-segment reservation. Shared between the
    /// classifier and IR-walker paths.
    fn build_runtime(bytes: &[u8], const_end: u32) -> Result<InstantiateOutcome, WasmEvalError> {
        let mut config = Config::new();
        config.wasm_tail_call(true);
        let engine = Engine::new(&config).map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;
        let module =
            Module::new(&engine, bytes).map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;

        let mut linker = Linker::<HostState>::new(&engine);
        host_imports::register_host_imports(&mut linker)
            .map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;

        let mut store = Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| WasmEvalError::Wasmtime(e.to_string()))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmEvalError::Wasmtime("module missing exported `memory`".into()))?;
        store.data_mut().bind_memory(memory);
        if const_end > 0 {
            store.data_mut().bind_const_segment_end(const_end);
        }

        let main_typed = instance
            .get_typed_func::<i64, i64>(&mut store, "__main")
            .map_err(|e| WasmEvalError::Wasmtime(format!("get __main typed func: {e}")))?;
        Ok(InstantiateOutcome {
            store,
            instance,
            main_typed,
        })
    }

    /// Classifier path — match a known cmp_lua workload and emit via
    /// the hand-tuned per-variant lowering in `relon-codegen-wasm`.
    fn instantiate_classifier(
        source: &str,
        _node: Node,
        tree_walk: relon_evaluator::TreeWalkEvaluator,
        program: WasmProgram,
    ) -> Result<Self, WasmEvalError> {
        let bytes = lower(&program).map_err(WasmEvalError::Lower)?;
        let outcome = Self::build_runtime(&bytes, const_segment_end(&program))?;
        let arg_name = match program {
            WasmProgram::W12IncrementInt => "x".to_string(),
            _ => "n".to_string(),
        };
        Ok(Self {
            tree_walk: Box::new(tree_walk),
            source: source.to_string(),
            wasm: Some(WasmRuntime {
                store: Mutex::new(outcome.store),
                instance: outcome.instance,
                program: ProgramSource::Classifier(program),
                main_typed: outcome.main_typed,
                arg_name,
            }),
        })
    }

    /// Phase Z.4.0 — IR-walker path. Drives a successfully-lowered
    /// `LoweredEntry` through the walker, then through wasmtime.
    fn instantiate_walker(
        source: &str,
        tree_walk: relon_evaluator::TreeWalkEvaluator,
        walker: WalkerOutcome,
    ) -> Result<Self, WasmEvalError> {
        let outcome = Self::build_runtime(&walker.bytes, 0)?;
        Ok(Self {
            tree_walk: Box::new(tree_walk),
            source: source.to_string(),
            wasm: Some(WasmRuntime {
                store: Mutex::new(outcome.store),
                instance: outcome.instance,
                program: ProgramSource::IrWalker(walker.return_shape),
                main_typed: outcome.main_typed,
                arg_name: walker.arg_name,
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
            Some(rt) => program_returns_scalar_int(&rt.program),
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
        if !program_returns_scalar_int(&rt.program) {
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
fn program_returns_scalar_int(program: &ProgramSource) -> bool {
    match program {
        ProgramSource::Classifier(p) => match p {
            WasmProgram::W1IntSumRange
            | WasmProgram::W2DotProduct
            | WasmProgram::W4StringContains { .. }
            | WasmProgram::W5DictAccessInline
            | WasmProgram::W6ListSumPlusOne
            | WasmProgram::W7FibRecursionInline
            | WasmProgram::W8PolymorphicDispatchInline
            | WasmProgram::W9NestedMatrixInline
            | WasmProgram::W10ConfigEvalInline
            | WasmProgram::W12IncrementInt => true,
            WasmProgram::W3StringConcatInline => false,
            // Scope-cut variants never instantiate a wasm runtime, so
            // this arm is unreachable in practice — but a hard match
            // keeps the check exhaustive so adding a future return
            // shape forces a conscious decision.
            WasmProgram::W3StringConcat
            | WasmProgram::W5DictAccess
            | WasmProgram::W7FibRecursion
            | WasmProgram::W8PolymorphicDispatch
            | WasmProgram::W9NestedMatrix
            | WasmProgram::W10ConfigEval => false,
        },
        // Phase Z.4.0 — scalar-Int return: the i64 IS the user's
        // `Value::Int` directly, so the fast-path entry is
        // semantically meaningful.
        // Phase Z.4.1 — Dict-record return: the i64 is an arena
        // pointer the host needs to walk into a `Value::Dict`; the
        // fast-path entry would hand back the raw pointer (a
        // meaningless arena offset under the `wasmtime_fast` label),
        // so it's NOT semantically meaningful and must surface as
        // `false`. Matches the LLVM-side discipline (`has_fast_path`
        // is `false` for non-canonical-Ret-wrapper returns).
        ProgramSource::IrWalker(IrWalkerReturn::ScalarInt) => true,
        ProgramSource::IrWalker(IrWalkerReturn::DictRecord { .. }) => false,
        // Phase Z.4.2 — `List<Int>` return: the i64 is an absolute
        // data-segment pointer, which is meaningless under the
        // `wasmtime_fast` label (callers expect a scalar Int). Match
        // the LLVM-side discipline: non-canonical-Ret-wrapper returns
        // surface as `false`.
        ProgramSource::IrWalker(IrWalkerReturn::ListInt) => false,
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

        // Pack args. All currently-recognised programs declare a single
        // `#main(Int <name>)` parameter; the arg name is cached on the
        // runtime (classifier path picks per-variant; IR-walker path
        // reads it off the `MainParams` schema's first field).
        let arg_i64 = extract_named_int(&args, &rt.arg_name)?;

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
        match &rt.program {
            ProgramSource::Classifier(WasmProgram::W3StringConcatInline) => {
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
            // Phase Z.4.1 — Dict-record return. The i64 carries the
            // i32 arena pointer the walker emitted at the matching
            // `AllocRootRecord`; walk the schema layout to materialise
            // each field into the resulting `Value::Dict`. The arena
            // slice stays valid until the next `HostState::reset`, so
            // we read out every field before releasing the store
            // mutex.
            ProgramSource::IrWalker(IrWalkerReturn::DictRecord { schema, layout }) => {
                let record_base = (out as u64) as u32;
                let memory = rt
                    .instance
                    .get_memory(&mut *store, "memory")
                    .ok_or_else(|| io_err("wasm-eval: instance missing `memory` export"))?;
                let view = memory.data(&*store);
                let dict = decode_dict_record(record_base, schema, layout, view)?;
                Ok(dict)
            }
            // Phase Z.4.2 — `List<Int>` return. The i64 carries the
            // i32 absolute pointer to a `[len: u32 LE][pad: u32
            // zero][i64 elements...]` record installed as an active
            // data segment at module instantiate time (no arena
            // alloc, so the record stays valid across `reset`).
            // Decode the header + payload into a `Value::List`.
            ProgramSource::IrWalker(IrWalkerReturn::ListInt) => {
                let record_base = (out as u64) as u32;
                let memory = rt
                    .instance
                    .get_memory(&mut *store, "memory")
                    .ok_or_else(|| io_err("wasm-eval: instance missing `memory` export"))?;
                let view = memory.data(&*store);
                let list = decode_list_int_record(record_base, view)?;
                Ok(list)
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

/// Phase Z.4.0 — IR-walker driver. Runs the source through
/// `parse + analyze + lower_workspace_single` and feeds the resulting
/// IR module into [`relon_codegen_wasm::lower_ir_module`]. Returns
/// either:
///
/// - `Ok(WalkerOutcome)` — emit succeeded, the caller can drive it
///   through wasmtime,
/// - `Err(IrWalkerSkipReason::IrLoweringFailed)` — the IR pipeline
///   itself rejected the source (e.g. `UnsupportedTypeInMain` for a
///   Float-return), or
/// - `Err(IrWalkerSkipReason::WalkerScopeCut)` — the IR walker met
///   an op outside its envelope; the carrier tag groups the cut by
///   follow-up sub-phase (Z.4.1 Dict, Z.4.2 List, Z.4.3 Closure).
fn try_lower_ir_walker(node: &Node) -> Result<WalkerOutcome, IrWalkerSkipReason> {
    let analyzed = relon_analyzer::analyze(node);
    let lowered = relon_ir::lower_workspace_single(&analyzed, node)
        .map_err(|e| IrWalkerSkipReason::IrLoweringFailed(e.to_string()))?;
    // Resolve the arg name from `MainParams`. The IR walker only
    // accepts single-Int-param programs today; we mirror that
    // constraint here so the typed-func handle stays `TypedFunc<i64,
    // i64>` across both paths.
    let main_params = &lowered.main_schema.fields;
    if main_params.len() != 1 {
        return Err(IrWalkerSkipReason::WalkerScopeCut(
            "multi_param",
            "Z.4-multi-param",
        ));
    }
    let arg_name = main_params[0].name.clone();
    // Classify the return shape from the lowering output so the host
    // knows how to unpack the typed-func's i64 result. The walker
    // applies the same classification rule (canonical
    // `Ret { value: Int }` → ScalarInt; everything else → DictRecord)
    // and refuses sources whose shape it can't lower; we mirror the
    // classification here so a successful `lower_ir_module` always
    // pairs with a matching [`IrWalkerReturn`] variant.
    let return_shape = classify_walker_return_shape(&lowered.return_schema)
        .map_err(|tag| IrWalkerSkipReason::WalkerScopeCut(tag, "Z.4.1-dict-return"))?;
    let bytes = lower_ir_module(&lowered).map_err(|e| match e {
        relon_codegen_wasm::LowerError::UnsupportedOp(tag, reason) => {
            IrWalkerSkipReason::WalkerScopeCut(tag, reason.tag())
        }
        other => IrWalkerSkipReason::IrLoweringFailed(other.to_string()),
    })?;
    Ok(WalkerOutcome {
        bytes,
        arg_name,
        return_shape,
    })
}

/// Walker-side return-shape classifier — mirrors the codegen
/// `classify_return_shape` rule (scalar `Ret { value: Int }` →
/// `ScalarInt`; everything else → `DictRecord`). Re-derives the
/// per-field offset table so the host-side decode skips the
/// per-call layout re-walk.
fn classify_walker_return_shape(schema: &Schema) -> Result<IrWalkerReturn, &'static str> {
    if schema.name != MAIN_RETURN_SCHEMA_NAME {
        return Err("ret_schema_unexpected_name");
    }
    let is_canonical_wrapper = schema.fields.len() == 1
        && schema.fields[0].name == RETURN_VALUE_FIELD_NAME
        && matches!(schema.fields[0].ty, TypeRepr::Int);
    if is_canonical_wrapper {
        return Ok(IrWalkerReturn::ScalarInt);
    }
    // Z.4.2 — `List<Int>` return shape. The canonical
    // `Ret { value: List<Int> }` wrapper matches the same single-field
    // shape as the scalar wrapper; the discriminator is the element
    // type. Mirrors `classify_return_shape` on the codegen side so
    // both layers agree before any byte hits the wasm encoder.
    if schema.fields.len() == 1 && schema.fields[0].name == RETURN_VALUE_FIELD_NAME {
        if let TypeRepr::List { element } = &schema.fields[0].ty {
            if matches!(**element, TypeRepr::Int) {
                return Ok(IrWalkerReturn::ListInt);
            }
            // Other list element types still scope-cut — the codegen
            // side rejects them under `UnsupportedOpReason::ListLiteral`,
            // so reaching this branch means the IR pipeline somehow
            // accepted a shape the codegen will reject; surface a
            // descriptive tag instead of `DictRecord`-shaped misdecode.
            return Err("ret_list_non_int_elem");
        }
    }
    // Re-derive the layout — `SchemaLayout::offsets_for` walks the
    // same canonical-form rules the codegen side uses. Failures are
    // unexpected (the codegen would have already errored) but we
    // route them through the scope-cut path for symmetry.
    let layout = SchemaLayout::offsets_for(schema).map_err(|_| "ret_layout_unsupported")?;
    Ok(IrWalkerReturn::DictRecord {
        schema: schema.clone(),
        layout,
    })
}

/// Output of a successful IR-walker emit. Carries the wasm bytes plus
/// the resolved arg name + return-shape classification so the host
/// can pack `#main`'s named arg and unpack the typed-func result
/// uniformly with the classifier path.
struct WalkerOutcome {
    bytes: Vec<u8>,
    arg_name: String,
    return_shape: IrWalkerReturn,
}

/// Reason the IR-walker path skipped a source. Distinguishes upstream
/// IR-pipeline rejects (which the walker never had a chance to see)
/// from walker-level scope-cuts (Z.4.x follow-up).
enum IrWalkerSkipReason {
    /// The IR pipeline itself rejected the source. Wraps the
    /// `relon_ir::LoweringError` string so tracing logs stay readable
    /// without pulling the full type into the error surface.
    IrLoweringFailed(String),
    /// The walker met an op outside its Z.4.0 envelope. The first tag
    /// is the op's debug name; the second is the
    /// `UnsupportedOpReason::tag()` follow-up-phase grouping.
    WalkerScopeCut(&'static str, &'static str),
}

/// Z.4.1 — decode a Dict-shape return record. The walker stored each
/// scalar field via `i64.store` at `record_base + offset`; the host
/// walks the schema layout in declaration order and reads each field
/// back out of linear memory. Closure-typed fields stay scope-cut at
/// codegen time (Z.4.3), so this decode only handles the scalar
/// surface (`Int`, `Bool` — String / Float / nested-schema fields
/// would each need their own decode arm + matching `StoreFieldAtRecord`
/// support in the walker).
///
/// Mirrors the LLVM-side `read_record_into_map` /
/// `read_value_from_reader` contract: each field's `name` keys into
/// the resulting `Value::branded_dict` map; the schema name passes
/// through as the brand.
fn decode_dict_record(
    record_base: u32,
    schema: &Schema,
    layout: &OffsetTable,
    view: &[u8],
) -> Result<Value, RuntimeError> {
    let mut map: HashMap<String, Value> = HashMap::with_capacity(schema.fields.len());
    for (i, field) in schema.fields.iter().enumerate() {
        let layout_field = layout.fields.get(i).ok_or_else(|| {
            io_err(format!(
                "wasm-eval Dict decode: layout missing field {} (schema/layout desync)",
                field.name
            ))
        })?;
        let abs_off = record_base as usize + layout_field.offset;
        let value = match &field.ty {
            TypeRepr::Int => {
                let end = abs_off.checked_add(8).ok_or_else(|| {
                    io_err(format!(
                        "wasm-eval Dict decode: Int offset overflow at field `{}`",
                        field.name
                    ))
                })?;
                if end > view.len() {
                    return Err(io_err(format!(
                        "wasm-eval Dict decode: field `{}` Int read out of memory \
                         (off={abs_off}, mem={})",
                        field.name,
                        view.len()
                    )));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&view[abs_off..end]);
                Value::Int(i64::from_le_bytes(bytes))
            }
            other => {
                // Codegen's `classify_return_shape` should have
                // refused non-Int Dict-return fields before we
                // reach here; surface a defensive `Unsupported`
                // for safety.
                return Err(RuntimeError::Unsupported {
                    reason: format!(
                        "wasm-eval Dict decode: field `{}` type {other:?} \
                         not supported (Z.4.1 — Int-only)",
                        field.name
                    ),
                });
            }
        };
        map.insert(field.name.clone(), value);
    }
    // The anon-Dict-return path's synthesised schema reuses the
    // canonical `Ret` schema name, but the user surface is bare
    // `Dict { ... }` — the tree-walker reference produces an
    // unbranded dict, so we mirror that here. (Branded user-defined
    // dict returns — `#main(...) -> User { ... }` — re-use the
    // user's schema name and stay branded; that path needs its own
    // arm once Z.4.1+ extends the walker to user-Schema returns.)
    let brand = if schema.name == MAIN_RETURN_SCHEMA_NAME {
        None
    } else {
        Some(schema.name.clone())
    };
    Ok(Value::branded_dict(map, brand))
}

/// Z.4.2 — decode a `List<Int>`-shape return record. The walker
/// installs the record as an active data segment whose layout is
/// `[len: u32 LE][pad: u32 zero][i64 elements...]`. We read the
/// header back, bounds-check the payload against the memory view,
/// and wrap the elements as a `Value::List` (with `Value::Int` per
/// element).
///
/// Mirrors the LLVM-side List<Int> decode: same header layout, same
/// pad placement, same little-endian element order. A regression in
/// the encoder's `encode_const_list_int_record` would surface here
/// as either a header mismatch (wrong `len`) or a misaligned i64
/// read (wrong values).
fn decode_list_int_record(record_base: u32, view: &[u8]) -> Result<Value, RuntimeError> {
    let header_start = record_base as usize;
    let header_end = header_start.checked_add(8).ok_or_else(|| {
        io_err(format!(
            "wasm-eval List<Int> decode: header offset overflow (base={record_base})"
        ))
    })?;
    if header_end > view.len() {
        return Err(io_err(format!(
            "wasm-eval List<Int> decode: header out of bounds (base={record_base}, mem={})",
            view.len()
        )));
    }
    let len = u32::from_le_bytes(
        view[header_start..header_start + 4]
            .try_into()
            .expect("8-byte header slice"),
    ) as usize;
    // The 4-byte pad lives at `view[header_start+4 .. header_start+8]`; we
    // skip it and read the i64 payload starting at `header_start + 8`.
    let payload_start = header_start + 8;
    let payload_end = payload_start
        .checked_add(8usize.saturating_mul(len))
        .ok_or_else(|| {
            io_err(format!(
                "wasm-eval List<Int> decode: payload range overflow (len={len})"
            ))
        })?;
    if payload_end > view.len() {
        return Err(io_err(format!(
            "wasm-eval List<Int> decode: payload out of bounds (len={len}, mem={})",
            view.len()
        )));
    }
    let mut elements: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        let elem_off = payload_start + i * 8;
        let v = i64::from_le_bytes(
            view[elem_off..elem_off + 8]
                .try_into()
                .expect("8-byte element slice"),
        );
        elements.push(Value::Int(v));
    }
    Ok(Value::List(elements.into()))
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
