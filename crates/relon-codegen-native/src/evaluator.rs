//! `CraneliftAotEvaluator` — the runtime façade for the cranelift
//! AOT backend.
//!
//! Construction does parse + analyze + lower (or pulls the IR out of
//! a `CacheEntry`), runs the codegen pass, finalizes the JIT module,
//! and stashes the resulting raw function pointer alongside its
//! per-call sandbox state. `run_main` materialises an arg vector,
//! resets the trap slot, invokes the JIT through a `catch_unwind`
//! shield, and translates any captured trap code into a typed
//! `RuntimeError`.
//!
//! v5-beta-1 supports the narrow `#main(Int...) -> Int` shape only;
//! every other `Evaluator` method returns
//! `RuntimeError::Unsupported`. The `AutoEvaluator` wrapper in the
//! `relon` facade keeps the tree-walker available for those code
//! paths, so callers never see a hard failure outside `run_main`.

use std::collections::HashMap;
use std::sync::Arc;

use cranelift_jit::JITModule;

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::{Node, TokenRange};

use crate::cache::CacheEntry;
use crate::codegen::{self, CompiledModule};
use crate::error::CraneliftError;
use crate::sandbox::{CapabilityVtable, SandboxConfig, SandboxState, TrapKind};

/// Type alias for the raw `extern "C"` entry the JIT produced. Five
/// args fit our v5-beta-1 envelope (`#main(Int x, Int y, Int z, Int w)`);
/// callers that need more arity than that surface
/// `UnsupportedSignature` long before the dispatch tries to call this.
type EntryFn = unsafe extern "C" fn(*const SandboxState, i64, i64, i64, i64) -> i64;

/// AOT evaluator backed by a cranelift JIT module.
pub struct CraneliftAotEvaluator {
    /// JIT module kept alive so the entry's machine code stays mapped.
    /// We never tear this down at run time; one module per evaluator
    /// is enough for v5-beta-1.
    _module: JITModule,
    /// Raw function pointer to the JIT'd `run_main`.
    entry_fn: EntryFn,
    /// Number of `Int` arguments the entry expects (i.e. the
    /// `#main(...)` arity).
    entry_arity: usize,
    /// Parameter names in declaration order. v5-beta-1 doesn't have
    /// the analyzer / parser hooked all the way through, so we fall
    /// back to synthetic names (`arg0` / `arg1` / ...) when the IR
    /// path can't surface real names.
    param_names: Vec<String>,
    /// Source range of the entry `#main` directive.
    entry_range: TokenRange,
    /// Per-call sandbox state. Wrapped in `Arc` so concurrent
    /// `run_main` invocations from multiple threads can hand the JIT
    /// the same pointer without contention on the underlying
    /// allocation; the few atomic fields synchronise updates.
    sandbox_state: Arc<SandboxState>,
}

// SAFETY: The JIT-emitted code is reentrant and the `SandboxState`
// fields that get mutated across calls (deadline / trap_code) are
// `AtomicI64` / `AtomicU64`. `JITModule` itself is `Send + Sync` in
// cranelift's current public surface.
unsafe impl Send for CraneliftAotEvaluator {}
unsafe impl Sync for CraneliftAotEvaluator {}

impl CraneliftAotEvaluator {
    /// Drive the full pipeline: parse + analyze + lower + cranelift
    /// codegen + JIT finalize.
    pub fn from_source(src: &str) -> Result<Self, CraneliftError> {
        let ir = Self::lower_source(src)?;
        let arity = ir_param_count(&ir)?;
        let sandbox_cfg = SandboxConfig::default();
        Self::from_ir(ir, sandbox_cfg, default_param_names_for(arity))
    }

    /// Skip parse + analyze + lower; rebuild a JIT module from the
    /// cached IR. Slower than a true binary cache (we still re-JIT)
    /// but already much faster than `from_source` because parse +
    /// analyze + lower commonly dominate cold-start.
    pub fn from_cache(entry: CacheEntry) -> Result<Self, CraneliftError> {
        let arity = ir_param_count(&entry.ir)?;
        Self::from_ir(entry.ir, entry.sandbox, default_param_names_for(arity))
    }

    /// Internal helper: lower a source string into an IR module. Mirrors
    /// `relon_codegen_wasm::WasmAotEvaluator::compile_source` minus the
    /// schema layout step.
    fn lower_source(src: &str) -> Result<relon_ir::ir::Module, CraneliftError> {
        let ast =
            relon_parser::parse_document(src).map_err(|e| CraneliftError::Parse(e.to_string()))?;
        let analyzed = relon_analyzer::analyze(&ast);
        if analyzed.has_errors() {
            let err_count = analyzed
                .diagnostics
                .iter()
                .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                .count();
            return Err(CraneliftError::Analyze(err_count));
        }
        let lowered = relon_ir::lower_workspace_single(&analyzed, &ast)
            .map_err(|e| CraneliftError::Lowering(e.to_string()))?;
        Ok(lowered.module)
    }

    /// Compile from a pre-lowered IR module. Public for v5-beta-1
    /// because the existing IR-emit pipeline assumes a wasm-side
    /// buffer + schema protocol the cranelift backend doesn't speak
    /// yet; tests and benchmarks therefore hand-build narrow IR
    /// modules and feed them in directly. v5-beta-2 wires the
    /// per-file analyzer / lowering pipeline through `from_source`.
    pub fn from_ir_direct(
        ir: relon_ir::ir::Module,
        sandbox_cfg: SandboxConfig,
        param_names: Vec<String>,
    ) -> Result<Self, CraneliftError> {
        Self::from_ir(ir, sandbox_cfg, param_names)
    }

    /// Compile from a pre-lowered IR module.
    fn from_ir(
        ir: relon_ir::ir::Module,
        sandbox_cfg: SandboxConfig,
        param_names: Vec<String>,
    ) -> Result<Self, CraneliftError> {
        let compiled = codegen::compile_module(&ir, &sandbox_cfg)?;
        let CompiledModule {
            module,
            entry_fn_id,
            entry_arity,
            entry_range,
        } = compiled;

        let raw_ptr = module.get_finalized_function(entry_fn_id);
        // SAFETY: JIT-finalized function pointers are stable for the
        // module's lifetime; we keep the module alive on `Self`.
        let entry_fn: EntryFn = unsafe { std::mem::transmute(raw_ptr) };

        // v5-beta-1: cap_bit width 64 mirrors the wasm-AOT side's
        // `relon_caps_avail` u64 bitmap shape. Hosts that register a
        // higher cap_bit cause `register` to grow the vector.
        let capabilities = Arc::new(CapabilityVtable::with_capacity(64));
        let sandbox_state = Arc::new(SandboxState::new(capabilities));
        sandbox_state.entry_range.set(entry_range);

        Ok(Self {
            _module: module,
            entry_fn,
            entry_arity,
            param_names,
            entry_range,
            sandbox_state,
        })
    }

    /// Replace the capability vtable wholesale. The new vtable is
    /// wired into a fresh [`SandboxState`] that inherits the entry
    /// range; the caller resets the deadline separately if needed.
    ///
    /// v5-beta-1 only supports `&mut self` reconfiguration because
    /// the JIT module's state pointer is captured at compile time;
    /// hosts that need to vary capabilities per call wrap the
    /// evaluator in their own `Mutex<CraneliftAotEvaluator>` and
    /// take the lock before each `run_main` invocation.
    pub fn install_capabilities_mut(&mut self, capabilities: Arc<CapabilityVtable>) {
        let new_state = SandboxState::new(capabilities);
        new_state.entry_range.set(self.entry_range);
        self.sandbox_state = Arc::new(new_state);
    }

    /// Configure the per-call wall-clock deadline. Pass
    /// `std::time::Duration::MAX` (or any value that overflows the
    /// nanos-as-i64 budget) to disable.
    pub fn set_deadline(&self, deadline: std::time::Duration) {
        self.sandbox_state.set_deadline(deadline);
    }

    /// Number of `#main` arguments expected.
    pub fn arity(&self) -> usize {
        self.entry_arity
    }

    /// Names of the declared `#main` parameters in declaration order.
    /// v5-beta-1 returns synthetic `arg0` / `arg1` / ... names because
    /// the IR pass doesn't surface parameter names to this layer.
    pub fn param_names(&self) -> &[String] {
        &self.param_names
    }

    /// Internal: invoke the JIT entry with the supplied i64 args.
    /// Uses `catch_unwind` to convert panics raised by cranelift
    /// trap instructions into typed `RuntimeError`s.
    fn invoke_entry(&self, args: [i64; 4]) -> Result<i64, RuntimeError> {
        self.sandbox_state.reset_trap();
        let state_ptr: *const SandboxState = Arc::as_ptr(&self.sandbox_state);
        let entry = self.entry_fn;

        // `catch_unwind` requires `UnwindSafe`; raw pointers + i64s
        // are inherently safe, and the JIT entry has no Rust state
        // tied to it. The `Cell<TokenRange>` field inside
        // `SandboxState` makes the type `!UnwindSafe`, so we wrap
        // the closure in `AssertUnwindSafe` — we set the entry_range
        // at construction time and don't mutate it from the JIT, so
        // a panic mid-call can't leave the Cell in an inconsistent
        // intermediate state.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            entry(state_ptr, args[0], args[1], args[2], args[3])
        }));

        match result {
            Ok(v) => {
                let code = self.sandbox_state.trap_code();
                if code != 0 {
                    return Err(TrapKind::from_code(code as u8).to_runtime_error(self.entry_range));
                }
                Ok(v)
            }
            Err(payload) => {
                // The cranelift trap path on unix raises SIGILL ->
                // panic via the runtime's signal handler. Try to
                // surface the captured TrapCode.
                let code = self.sandbox_state.trap_code();
                let _ = payload;
                if code != 0 {
                    Err(TrapKind::from_code(code as u8).to_runtime_error(self.entry_range))
                } else {
                    Err(RuntimeError::Unsupported {
                        reason: "cranelift-native: JIT entry panicked without a recorded trap code"
                            .into(),
                    })
                }
            }
        }
    }
}

/// Inspect the IR module's entry function and return its parameter
/// count. Used by both `from_source` and `from_cache` to pre-validate
/// the expected arity.
fn ir_param_count(ir: &relon_ir::ir::Module) -> Result<usize, CraneliftError> {
    let idx = ir
        .entry_func_index
        .ok_or_else(|| CraneliftError::Lowering("module has no entry function".into()))?;
    Ok(ir.funcs[idx].params.len())
}

/// Synthesise `arg0`..`argN` placeholder names. v5-beta-1 doesn't
/// route the analyzer's parameter names through to this point; the
/// `AutoEvaluator` wrapper consults the tree-walker's `#main`
/// signature for argument-binding purposes, so synthetic names here
/// are only ever observed by direct callers of the cranelift
/// backend.
fn default_param_names_for(arity: usize) -> Vec<String> {
    (0..arity).map(|i| format!("arg{i}")).collect()
}

impl Evaluator for CraneliftAotEvaluator {
    fn eval(&self, _node: &Node, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "cranelift-native AOT backend: `eval` requires AST access; use the tree-walking backend instead".to_string(),
        })
    }

    fn eval_root(&self, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "cranelift-native AOT backend: `eval_root` requires AST access; use the tree-walking backend instead".to_string(),
        })
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        if args.len() > 4 {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native v5-beta-1 supports up to 4 #main args; got {}",
                    args.len()
                ),
            });
        }
        if args.len() != self.entry_arity {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native: #main expects {} arg(s), got {}",
                    self.entry_arity,
                    args.len()
                ),
            });
        }

        // Materialise args into a fixed [i64; 4] array, padding with
        // zero. v5-beta-1 only accepts `Int` arguments; other shapes
        // surface as `MainArgTypeMismatch` so the diagnostic mirrors
        // the tree-walker's.
        let mut argv = [0i64; 4];
        for (i, name) in self.param_names.iter().enumerate() {
            let value = args.get(name).or_else(|| {
                // Hosts that don't know the synthetic names can also
                // pass keyed by positional `argN`.
                args.get(&format!("arg{i}"))
            });
            let value = value.ok_or_else(|| RuntimeError::MissingMainArg {
                name: name.clone(),
                range: self.entry_range,
            })?;
            match value {
                Value::Int(v) => argv[i] = *v,
                other => {
                    return Err(RuntimeError::MainArgTypeMismatch {
                        name: name.clone(),
                        expected: "Int".to_string(),
                        found: other.type_name().to_string(),
                        range: self.entry_range,
                    })
                }
            }
        }

        let result_i64 = self.invoke_entry(argv)?;
        Ok(Value::Int(result_i64))
    }

    fn force_thunk(&self, _thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "cranelift-native AOT backend: thunks are not represented in JIT code"
                .to_string(),
        })
    }

    fn invoke_closure(
        &self,
        _closure: &ClosureData,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "cranelift-native AOT backend: first-class closures land in v5-beta-2"
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Send + Sync sanity check so the AutoEvaluator path can hold a
    /// `Box<dyn Evaluator>` without surprises.
    #[test]
    fn cranelift_evaluator_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CraneliftAotEvaluator>();
    }
}
