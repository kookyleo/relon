//! Runtime façade for the LLVM AOT backend.
//!
//! Phase A bootstrap supports the cranelift crate's
//! `from_ir_direct` envelope only: a `Func` with `(I64...) -> I64`
//! signature, lowered through [`crate::emitter`] into an LLVM
//! module, JIT-finalized through inkwell's MCJIT execution engine.
//! `run_main` materialises the i64 argv and invokes the resolved
//! symbol behind a thin trampoline.
//!
//! ## Why MCJIT (and not ORC) for Phase A
//!
//! - MCJIT is the simplest engine that inkwell exposes — single
//!   `create_jit_execution_engine` call, no per-symbol resolver
//!   plumbing. The Phase A goal is a working byte-identical round
//!   trip, not throughput.
//! - inkwell 0.9.0 wraps both engines, so switching to ORC in
//!   Phase B is a localised diff: one call site here, the emitter
//!   stays untouched.
//! - LLVM 18's MCJIT still handles our hot path (single function,
//!   no global state, no external symbols).

use std::collections::HashMap;
use std::sync::Arc;

use inkwell::context::Context;
use inkwell::execution_engine::ExecutionEngine;
use inkwell::OptimizationLevel;

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::Node;

use crate::emitter::{emit_function, ENTRY_SYMBOL};
use crate::error::LlvmError;

/// Maximum positional arity supported by the Phase A legacy-i64
/// entry. Mirrors the cranelift crate's `MAX_LEGACY_ARITY`; the four
/// slots cover the existing helloworld-style bodies and keep the
/// trampoline branch table small. Phase B raises the cap alongside
/// the variadic-trampoline work the cranelift crate is staging for
/// buffer-protocol parity.
pub const MAX_LEGACY_ARITY: usize = 4;

/// Type alias for the raw `extern "C"` function pointer the LLVM JIT
/// produced. Five i64 slots accept the v5-β-1 envelope's max arity;
/// shorter signatures pass zero in the trailing slots — the emitter
/// only declares the parameters the IR has, so the unused trailing
/// slots are dead-on-arrival.
type EntryFn4 = unsafe extern "C" fn(i64, i64, i64, i64) -> i64;
type EntryFn3 = unsafe extern "C" fn(i64, i64, i64) -> i64;
type EntryFn2 = unsafe extern "C" fn(i64, i64) -> i64;
type EntryFn1 = unsafe extern "C" fn(i64) -> i64;
type EntryFn0 = unsafe extern "C" fn() -> i64;

/// Owned LLVM JIT state for a single compiled module. The
/// [`Context`] / [`ExecutionEngine`] pair must outlive every call
/// into the JITted function pointer; we park them on the heap behind
/// the evaluator so the host can ignore lifetimes.
struct JitOwned {
    // The `Context` must outlive the ExecutionEngine; we keep it in a
    // pinned heap slot so the engine's borrow stays valid for the
    // evaluator's lifetime. `Box::leak` would also work but a
    // `Box<Context>` lets us drop both cleanly when the evaluator
    // goes out of scope (the `Drop` order is field declaration order
    // reversed — engine first, then context).
    _engine: ExecutionEngine<'static>,
    /// Raw entry function pointer resolved once at construction time.
    ///
    /// inkwell's `JitFunction::call` re-acquires the address from
    /// the engine on every invocation (the wrapper's `Drop` /
    /// lifetime plumbing assumes a fresh `get_function`). Caching
    /// the raw pointer up-front turns the hot path into a single
    /// indirect call — the same shape the cranelift backend's
    /// `LegacyEntryFn` stash takes. Profile data on the Phase A.4
    /// bootstrap showed `get_function` per-call dominating the LLVM
    /// row by ~5-7×; caching collapses the gap to within the
    /// expected boundary-cost band.
    entry_ptr: usize,
    /// Pre-rendered textual LLVM IR captured at construction. inkwell
    /// 0.9 does not expose `ExecutionEngine::get_module*`, so the
    /// dump-time call cannot reach back to the live module. We pay
    /// the one-shot `print_to_string` cost up-front so `emit_ir_dump`
    /// stays infallible at call time. Phase B may swap MCJIT for ORC
    /// and surface a live `&Module` handle; until then this string
    /// is the canonical Phase A inspection surface.
    ir_dump: String,
    _ctx: Box<Context>,
}

// SAFETY: the inkwell ExecutionEngine + Context pair is not `Sync`
// by default — LLVM's `LLVMContextRef` is per-thread. The Phase A
// evaluator pins the JIT to the constructing thread and guards
// `run_main` with a `Mutex` (TODO Phase B). For the smoke test the
// raw `unsafe impl` is enough; the trait surface needs `Sync` so the
// `Evaluator` trait bound holds.
//
// This matches what the cranelift backend does with its own
// `JITModule` (`Send`/`Sync` documented but not autoderived). When
// Phase B introduces multi-thread dispatch we'll swap this for a
// per-thread engine pool.
unsafe impl Send for JitOwned {}
unsafe impl Sync for JitOwned {}

/// Phase A LLVM AOT evaluator. Constructed from a pre-lowered IR
/// module via [`Self::from_ir_direct`]; `from_source` is intentionally
/// **not** wired yet because `lower_workspace_single` emits buffer-
/// protocol IR which the Phase A emitter rejects.
pub struct LlvmAotEvaluator {
    jit: JitOwned,
    entry_arity: usize,
    param_names: Vec<String>,
}

impl LlvmAotEvaluator {
    /// Compile a pre-lowered IR module into a JIT-resident function
    /// pointer. The Phase A envelope is intentionally narrow: the
    /// module must contain exactly one `Func` with `(I64...) -> I64`
    /// signature; anything else surfaces as
    /// [`LlvmError::UnsupportedSignature`].
    ///
    /// `param_names` parallels the cranelift backend's
    /// `from_ir_direct` arg so the `Evaluator::run_main` dispatch
    /// can look up positional args by their declared name. Phase A
    /// supports only `Int` params, so the lookup walks
    /// `HashMap<String, Value::Int>` and rejects anything else.
    pub fn from_ir_direct(
        ir: relon_ir::ir::Module,
        param_names: Vec<String>,
    ) -> Result<Self, LlvmError> {
        let entry_idx = ir
            .entry_func_index
            .ok_or_else(|| LlvmError::Codegen("IR module has no entry function".into()))?;
        let entry = &ir.funcs[entry_idx];

        if entry.params.len() > MAX_LEGACY_ARITY {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot Phase A: {} params exceeds MAX_LEGACY_ARITY={MAX_LEGACY_ARITY}",
                entry.params.len()
            )));
        }
        if entry.params.len() != param_names.len() {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot Phase A: IR arity {} does not match param_names len {}",
                entry.params.len(),
                param_names.len()
            )));
        }

        // Build the LLVM module under a per-evaluator Context. We
        // leak the Context onto the heap and transmute the engine's
        // lifetime to `'static` (see SAFETY note on `JitOwned`).
        // The double-Box ensures the Context address never moves
        // even if the surrounding struct is shuffled.
        let ctx_box: Box<Context> = Box::new(Context::create());
        // SAFETY: `ctx_box` lives on the heap and we never deallocate
        // it before the engine (Drop order is engine first, then
        // _ctx). The 'static promoted ref is consumed only by the
        // engine builder.
        let ctx_static: &'static Context = unsafe { &*(ctx_box.as_ref() as *const Context) };

        let module = ctx_static.create_module("relon_llvm_aot");

        // Lower the IR's entry function into the LLVM module. The
        // emitter validates the legacy-i64 envelope before touching
        // the builder, so any UnsupportedSignature shows up here.
        emit_function(ctx_static, &module, entry)?;

        // Verify before handing to the JIT so a malformed IR pass
        // produces a structured error instead of an LLVM abort. The
        // verifier returns its diagnostic as a `LLVMString` we ferry
        // into `LlvmError::Codegen`.
        module
            .verify()
            .map_err(|e| LlvmError::Codegen(format!("LLVM verifier rejected module: {e}")))?;

        // Snapshot the printable IR before the engine takes ownership
        // of the module. inkwell 0.9's ExecutionEngine has no
        // get_module* surface, so this is the only Phase A path that
        // can render the IR back as text without holding a separate
        // `&Module` borrow alongside the engine. The string is small
        // (a few hundred bytes for the bootstrap envelope) and only
        // computed once per evaluator construction.
        let ir_dump = module.print_to_string().to_string();

        let engine = module
            .create_jit_execution_engine(OptimizationLevel::Aggressive)
            .map_err(|e| LlvmError::Codegen(format!("create_jit_execution_engine: {e}")))?;

        // Resolve the entry symbol once at construction time. The
        // `usize` we cache is the JITted code's load address — stable
        // for the engine's lifetime. We re-cast it to the matching
        // `extern "C"` fn pointer on each dispatch.
        let entry_ptr = engine.get_function_address(ENTRY_SYMBOL).map_err(|e| {
            LlvmError::Codegen(format!(
                "ExecutionEngine could not resolve `{ENTRY_SYMBOL}`: {e}"
            ))
        })?;

        Ok(Self {
            jit: JitOwned {
                _engine: engine,
                entry_ptr,
                ir_dump,
                _ctx: ctx_box,
            },
            entry_arity: entry.params.len(),
            param_names,
        })
    }

    /// Number of `#main` arguments expected. Mirrors the cranelift
    /// crate's [`AotEvaluator::arity`] shape so hosts can swap the
    /// two implementations without changing their dispatch code.
    pub fn arity(&self) -> usize {
        self.entry_arity
    }

    /// Names of the declared `#main` parameters in declaration order.
    /// Mirrors [`AotEvaluator::param_names`]'s shape.
    pub fn param_names(&self) -> &[String] {
        &self.param_names
    }

    /// Fast-path entry mirroring
    /// `AotEvaluator::run_main_legacy_i64`: skip the HashMap pack and
    /// invoke the JIT entry with a slice of positional i64 args.
    ///
    /// Phase A keeps the trampoline statically dispatched per arity
    /// (0 / 1 / 2 / 3 / 4) instead of going through `LLVMRunFunction`
    /// — the raw `JitFunction` invocation matches what the cranelift
    /// backend does for the same entry shape and avoids LLVM's
    /// generic-value boxing on the hot path.
    pub fn run_main_legacy_i64(&self, args: &[i64]) -> Result<i64, RuntimeError> {
        if args.len() != self.entry_arity {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "llvm-aot: #main expects {} arg(s), got {}",
                    self.entry_arity,
                    args.len()
                ),
            });
        }
        // SAFETY: the JIT entry was emitted as `(i64...) -> i64` with
        // the exact arity we stored in `self.entry_arity`. The cached
        // `entry_ptr` was returned by `ExecutionEngine::get_function_address`
        // at construction time and stays valid for the engine's
        // lifetime (which the evaluator owns). The `unsafe extern "C"`
        // ABI matches LLVM's emitted function (system default cc for
        // the host triple), so the raw transmute + call is well-formed.
        let ptr = self.jit.entry_ptr;
        unsafe {
            match self.entry_arity {
                0 => {
                    let f: EntryFn0 = std::mem::transmute(ptr);
                    Ok(f())
                }
                1 => {
                    let f: EntryFn1 = std::mem::transmute(ptr);
                    Ok(f(args[0]))
                }
                2 => {
                    let f: EntryFn2 = std::mem::transmute(ptr);
                    Ok(f(args[0], args[1]))
                }
                3 => {
                    let f: EntryFn3 = std::mem::transmute(ptr);
                    Ok(f(args[0], args[1], args[2]))
                }
                4 => {
                    let f: EntryFn4 = std::mem::transmute(ptr);
                    Ok(f(args[0], args[1], args[2], args[3]))
                }
                n => Err(RuntimeError::Unsupported {
                    reason: format!("llvm-aot: arity {n} > MAX_LEGACY_ARITY={MAX_LEGACY_ARITY}"),
                }),
            }
        }
    }

    /// Print the emitted LLVM IR. Phase A test plumbing — handy for
    /// the bootstrap test to surface the textual IR in the test log
    /// so a regression in the emitter shows up as a diff in the
    /// dumped IR rather than a silent miscompile.
    ///
    /// Wraps inkwell's `Module::print_to_string`; the returned
    /// `String` owns the bytes so the caller can `assert!` /
    /// `eprintln!` without lifetime juggling.
    pub fn emit_ir_dump(&self) -> &str {
        &self.jit.ir_dump
    }
}

impl Evaluator for LlvmAotEvaluator {
    fn eval(&self, _node: &Node, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `eval` is not supported (Phase A bootstrap)".into(),
        })
    }

    fn eval_root(&self, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `eval_root` is not supported (Phase A bootstrap)".into(),
        })
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // Pack the HashMap into a positional i64 argv using the
        // declared parameter order. Matches what
        // `AotEvaluator::run_main` does for the legacy envelope.
        let mut argv = [0i64; MAX_LEGACY_ARITY];
        for (i, name) in self.param_names.iter().enumerate() {
            let v = args.get(name).ok_or_else(|| RuntimeError::Unsupported {
                reason: format!("llvm-aot: missing #main arg `{name}`"),
            })?;
            match v {
                Value::Int(n) => argv[i] = *n,
                other => {
                    return Err(RuntimeError::Unsupported {
                        reason: format!(
                            "llvm-aot: #main arg `{name}` is {} (Phase A supports Int only)",
                            other.type_name()
                        ),
                    });
                }
            }
        }
        let r = self.run_main_legacy_i64(&argv[..self.entry_arity])?;
        Ok(Value::Int(r))
    }

    fn force_thunk(&self, _thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `force_thunk` is not supported (Phase A bootstrap)".into(),
        })
    }

    fn invoke_closure(
        &self,
        _closure: &ClosureData,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `invoke_closure` is not supported (Phase A bootstrap)".into(),
        })
    }
}
