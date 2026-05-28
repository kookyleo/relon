//! Unified evaluator construction surface.
//!
//! [`EvaluatorBuilder`] is the open-the-box construction path for hosts
//! that just want a `Box<dyn Evaluator>`. It centralises every knob —
//! source location, backend selection, trust posture, host-registered
//! native fns — so the facade no longer needs to leak
//! `TreeWalkEvaluator` / `Context` / `Scope` through wildcard
//! re-exports of the downstream crates.
//!
//! The builder is deliberately focused: it covers the common
//! `from_str` / `from_file` + backend-swap shape. Hosts that need
//! finer control (custom `ModuleResolver` chain, per-invocation
//! capability flips, low-level `Context::with_root` wiring) still
//! reach into [`crate::ResolverChainLoader`] + the lower-level
//! `relon-eval-api` / `relon-evaluator` crates directly — same as
//! the in-tree `relon-cli` / `relon-wasm-bindings` / `relon-lsp` binaries do.
//!
//! ## Trust posture
//!
//! Mirrors the [`crate::from_str`] / [`crate::from_str_trusted`]
//! split: [`TrustLevel::Sandboxed`] (default) refuses filesystem
//! `#import` and capability-gated host fns; [`TrustLevel::Trusted`]
//! grants every capability and mounts the trusted filesystem +
//! remote-HTTP resolvers. Host fns the builder registers ride the
//! same capability gate the script-side `Capabilities` budget
//! enforces.
//!
//! ## Backend coverage
//!
//! Native-fn registration is meaningful only for the tree-walker
//! (i.e. [`Backend::TreeWalk`] and the tree-walker side of
//! [`Backend::Auto`]). Backends that lower the source to native
//! code or bytecode ([`Backend::CraneliftAot`], [`Backend::Bytecode`])
//! cannot dispatch host-registered fns today — calling
//! [`EvaluatorBuilder::register_native_fn`] under those backends
//! surfaces a [`BackendError::UnsupportedFeature`] at
//! [`EvaluatorBuilder::build`] time so the failure is loud rather
//! than silent.

use relon_eval_api::{Evaluator, NativeFnGate, RelonFunction};
#[cfg(all(not(target_arch = "wasm32"), feature = "remote-http"))]
use relon_evaluator::module::RemoteHttpResolver;
use relon_evaluator::module::{FilesystemModuleResolver, ModuleResolver};
use relon_evaluator::{Capabilities, Context, TreeWalkEvaluator};
use relon_parser::parse_document;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::auto_evaluator::AutoEvaluator;
use crate::{Backend, BackendError};

/// Trust posture handed to [`EvaluatorBuilder::trust`]. Mirrors the
/// `TrustMode` the facade entry points use internally; surfaced
/// publicly so the open-the-box construction path can flip it
/// without writing a custom `Context`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrustLevel {
    /// Default. Refuses filesystem `#import` and every
    /// capability-gated native fn. Equivalent to
    /// `Capabilities::default()` plus a sandboxed module resolver
    /// chain (`std/*` only). Aligns with [`crate::from_str`]'s
    /// posture.
    #[default]
    Sandboxed,
    /// Grants every capability bit and mounts the trusted
    /// filesystem + (on native targets only) remote-HTTP module
    /// resolvers. Aligns with [`crate::from_str_trusted`]'s
    /// posture. Reserve for host-owned input.
    Trusted,
}

/// Source the builder evaluates. Either an in-memory string the host
/// supplies inline ([`EvaluatorBuilder::from_str`]) or a file the
/// builder reads at [`EvaluatorBuilder::build`] time
/// ([`EvaluatorBuilder::from_file`]).
enum Source {
    Inline(String),
    File(PathBuf),
}

/// Host-registered native fn pending insertion into the constructed
/// `Context`. Stored alongside the gate so the builder can stage
/// capability-aware registrations the same way `Context::register_fn`
/// does. See [`EvaluatorBuilder::register_native_fn`] /
/// [`EvaluatorBuilder::register_pure_native_fn`].
struct PendingNativeFn {
    name: String,
    gate: NativeFnGate,
    func: Arc<dyn RelonFunction>,
}

/// Open-the-box evaluator construction.
///
/// Typical usage:
///
/// ```no_run
/// use relon::{Backend, EvaluatorBuilder, TrustLevel};
///
/// let evaluator = EvaluatorBuilder::from_str("#main(Int x) -> Int { x + 1 }")
///     .backend(Backend::Auto)
///     .trust(TrustLevel::Sandboxed)
///     .build()
///     .expect("setup");
/// // `evaluator: Box<dyn relon_eval_api::Evaluator>` — drive with
/// // `run_main` / `eval_root` from the trait surface.
/// ```
pub struct EvaluatorBuilder {
    source: Source,
    backend: Backend,
    trust: TrustLevel,
    /// Host fns staged for registration on the tree-walker `Context`.
    /// Stays empty for the cranelift / bytecode backends since they
    /// cannot dispatch host fns; carrying values into a non-TreeWalk
    /// build surfaces `BackendError::UnsupportedFeature` so the
    /// failure is loud rather than silent.
    pending_fns: Vec<PendingNativeFn>,
}

impl EvaluatorBuilder {
    /// Build from an in-memory source string. Mirrors
    /// [`crate::value_from_str`]'s entry point: the file path
    /// reported in diagnostics is `<memory>` and module imports
    /// resolve relative to the current working directory.
    ///
    /// Naming intentionally shadows `std::str::FromStr::from_str` so
    /// the spelling matches [`crate::from_str`] / [`crate::value_from_str`];
    /// the builder is not a `FromStr` impl (it returns `Self`, not
    /// `Result`, and uses `Into<String>` to keep cheap-clone hosts
    /// allocation-free at the call site).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(source: impl Into<String>) -> Self {
        Self {
            source: Source::Inline(source.into()),
            backend: Backend::default(),
            trust: TrustLevel::default(),
            pending_fns: Vec::new(),
        }
    }

    /// Build from a file path. The source is read at [`Self::build`]
    /// time; the path is canonicalised so `#import` directives resolve
    /// relative to the file's parent directory (matching
    /// [`crate::value_from_file`]).
    pub fn from_file(path: impl AsRef<Path>) -> Self {
        Self {
            source: Source::File(path.as_ref().to_path_buf()),
            backend: Backend::default(),
            trust: TrustLevel::default(),
            pending_fns: Vec::new(),
        }
    }

    /// Select an evaluator backend. Defaults to [`Backend::Auto`].
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    /// Switch trust posture. Defaults to [`TrustLevel::Sandboxed`].
    pub fn trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    /// Register a host native fn with explicit capability requirements.
    /// Equivalent to `Context::register_fn` but staged so the builder
    /// can apply it during [`Self::build`].
    ///
    /// Only meaningful for tree-walker-backed builds
    /// ([`Backend::TreeWalk`] and the tree-walker side of
    /// [`Backend::Auto`]). Calling this under [`Backend::CraneliftAot`]
    /// / [`Backend::Bytecode`] surfaces
    /// [`BackendError::UnsupportedFeature`] at `build` time.
    pub fn register_native_fn(
        mut self,
        name: impl Into<String>,
        gate: NativeFnGate,
        func: Arc<dyn RelonFunction>,
    ) -> Self {
        self.pending_fns.push(PendingNativeFn {
            name: name.into(),
            gate,
            func,
        });
        self
    }

    /// Pure-fn convenience: defaults the gate to
    /// [`NativeFnGate::default`] (no capability required). Use this
    /// for deterministic host fns (`"args in, value out"`).
    pub fn register_pure_native_fn(
        self,
        name: impl Into<String>,
        func: Arc<dyn RelonFunction>,
    ) -> Self {
        self.register_native_fn(name, NativeFnGate::default(), func)
    }

    /// Assemble the configured evaluator. Returns `Box<dyn Evaluator>`
    /// so backend swap stays a runtime concern; the trait surface is
    /// the same five `&self` methods every backend implements.
    pub fn build(self) -> Result<Box<dyn Evaluator>, BackendError> {
        let EvaluatorBuilder {
            source,
            backend,
            trust,
            pending_fns,
        } = self;

        // Resolve the source to an owned string. File reads happen
        // here (not at construction time) so the host can pre-stage
        // a builder and only pay the I/O cost at `build` time.
        let source_string = match source {
            Source::Inline(s) => s,
            Source::File(path) => {
                let canonical_path = std::fs::canonicalize(&path).map_err(|e| {
                    BackendError::Parse(format!("file read {}: {}", path.display(), e))
                })?;
                std::fs::read_to_string(&canonical_path).map_err(|e| {
                    BackendError::Parse(format!("file read {}: {}", canonical_path.display(), e))
                })?
            }
        };

        // Host fns can only attach to the tree-walker. Reject early
        // on backends that can't dispatch them so the host sees the
        // mismatch instead of silently dropping the registration.
        if !pending_fns.is_empty() && !matches!(backend, Backend::Auto | Backend::TreeWalk) {
            return Err(BackendError::UnsupportedFeature(format!(
                "native-fn registration is only supported on Backend::Auto / Backend::TreeWalk; got {:?}",
                backend
            )));
        }

        match backend {
            Backend::Auto => {
                let auto = AutoEvaluator::new(&source_string)?;
                // Stage 1 wires register_native_fn into the
                // tree-walker side of AutoEvaluator. For now the
                // builder rejects host-fn registration under Auto
                // until AutoEvaluator exposes a fn-registration
                // shim; this keeps the public surface honest.
                if !pending_fns.is_empty() {
                    return Err(BackendError::UnsupportedFeature(
                        "native-fn registration under Backend::Auto is not yet implemented; use Backend::TreeWalk".to_string(),
                    ));
                }
                let _ = trust; // Auto's TrustLevel honouring lives upstream of this stage.
                Ok(Box::new(auto))
            }
            Backend::TreeWalk => {
                let tw = build_tree_walk(&source_string, trust, pending_fns)?;
                Ok(Box::new(tw))
            }
            #[cfg(feature = "cranelift-aot")]
            Backend::CraneliftAot => {
                let aot = relon_codegen_cranelift::AotEvaluator::from_source(&source_string)
                    .map_err(|e| BackendError::CraneliftAot(e.to_string()))?;
                Ok(Box::new(aot))
            }
            #[cfg(not(feature = "cranelift-aot"))]
            Backend::CraneliftAot => Err(BackendError::CraneliftAot(
                "this build was compiled without the `cranelift-aot` feature; rebuild with `--features cranelift-aot` to enable the backend"
                    .to_string(),
            )),
            Backend::Bytecode => {
                let bc = relon_bytecode::BytecodeEvaluator::from_source(&source_string)
                    .map_err(|e| BackendError::Bytecode(e.to_string()))?;
                Ok(Box::new(bc))
            }
            // Phase A LLVM-AOT does not yet ingest `from_source`
            // because `lower_workspace_single` emits buffer-protocol
            // IR which the Phase A emitter rejects. Surface a clear
            // not-implemented so hosts know to fall back to the
            // cranelift backend until Phase B.
            Backend::LlvmAot => Err(BackendError::LlvmAot(
                "Phase A bootstrap: `from_source` not wired yet — \
                 use `Backend::CraneliftAot` for source-driven AOT or \
                 construct an `LlvmAotEvaluator::from_ir_direct` against \
                 a pre-lowered IR module for the bootstrap envelope"
                    .to_string(),
            )),
        }
    }
}

/// Assemble a tree-walker honouring trust posture + staged host fns.
/// Mirrors [`crate::build_tree_walk_evaluator`] but threads
/// `TrustLevel` and host fns; kept in this module so the builder owns
/// the surface that depends on both.
fn build_tree_walk(
    source: &str,
    trust: TrustLevel,
    pending_fns: Vec<PendingNativeFn>,
) -> Result<TreeWalkEvaluator, BackendError> {
    let node = parse_document(source).map_err(|e| BackendError::Parse(e.to_string()))?;
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = match trust {
        TrustLevel::Sandboxed => Context::sandboxed(),
        TrustLevel::Trusted => Context::new(),
    }
    .with_root(node)
    .with_analyzed(Arc::clone(&analyzed));

    // Honour Trusted by granting all capabilities and prepending the
    // trusted-filesystem + remote-HTTP resolvers, matching the
    // assembly used by `crate::evaluate_source` in the trusted branch.
    if matches!(trust, TrustLevel::Trusted) {
        ctx.capabilities = Capabilities::all_granted();
        ctx.prepend_module_resolver(
            Arc::new(FilesystemModuleResolver::trusted()) as Arc<dyn ModuleResolver>
        );
        #[cfg(all(not(target_arch = "wasm32"), feature = "remote-http"))]
        ctx.prepend_module_resolver(Arc::new(RemoteHttpResolver::new()) as Arc<dyn ModuleResolver>);
    }

    TreeWalkEvaluator::prepare_in_place(&mut ctx);

    // Apply staged host fns after `prepare_in_place` so they cannot
    // be overwritten by the stdlib seeding. Stdlib names that
    // collide with a host-registered name are intentionally shadowed
    // by the host registration (this matches `Context::register_fn`'s
    // last-writer-wins semantics).
    for PendingNativeFn { name, gate, func } in pending_fns {
        ctx.register_fn(name, gate, func);
    }

    Ok(TreeWalkEvaluator::new(Arc::new(ctx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_eval_api::{NativeArgs, RuntimeError, Scope, Value};
    use relon_parser::TokenRange;

    #[test]
    fn from_str_default_runs_eval_root() {
        let evaluator = EvaluatorBuilder::from_str(r#"{ host: "x", port: 80 }"#)
            .build()
            .expect("build");
        let value = evaluator
            .eval_root(&Arc::new(Scope::default()))
            .expect("eval_root");
        match value {
            Value::Dict(d) => {
                assert_eq!(d.map.len(), 2);
            }
            other => panic!("expected dict, got {other:?}"),
        }
    }

    #[test]
    fn tree_walk_backend_runs_main() {
        let evaluator = EvaluatorBuilder::from_str("#main(Int x) -> Int\nx + 1")
            .backend(Backend::TreeWalk)
            .build()
            .expect("build");
        let mut args = std::collections::HashMap::new();
        args.insert("x".to_string(), Value::Int(41));
        let value = evaluator.run_main(args).expect("run_main");
        assert_eq!(value, Value::Int(42));
    }

    #[test]
    fn native_fn_registers_through_builder() {
        struct AddOne;
        impl RelonFunction for AddOne {
            fn call(&self, args: NativeArgs, range: TokenRange) -> Result<Value, RuntimeError> {
                let positional = args.into_positional();
                match positional.first() {
                    Some(Value::Int(n)) => Ok(Value::Int(n + 1)),
                    _ => Err(RuntimeError::TypeMismatch {
                        expected: "Int".to_string(),
                        found: "other".to_string(),
                        range,
                    }),
                }
            }
        }

        let evaluator = EvaluatorBuilder::from_str(r#"{ v: add_one(41) }"#)
            .backend(Backend::TreeWalk)
            .register_pure_native_fn("add_one", Arc::new(AddOne))
            .build()
            .expect("build");
        let value = evaluator
            .eval_root(&Arc::new(Scope::default()))
            .expect("eval_root");
        let Value::Dict(d) = value else {
            panic!("expected dict")
        };
        assert_eq!(d.map.get("v"), Some(&Value::Int(42)));
    }

    #[test]
    fn native_fn_rejected_on_bytecode_backend() {
        let result = EvaluatorBuilder::from_str("#main(Int x) -> Int\nx + 1")
            .backend(Backend::Bytecode)
            .register_pure_native_fn("noop", Arc::new(NoopFn))
            .build();
        // `Box<dyn Evaluator>` is not `Debug` so the `Result::expect_err`
        // shortcut is unavailable; pattern-match explicitly.
        match result {
            Err(BackendError::UnsupportedFeature(_)) => {}
            Err(other) => panic!("expected UnsupportedFeature, got {other:?}"),
            Ok(_) => panic!("expected builder to reject native fns under bytecode backend"),
        }
    }

    struct NoopFn;
    impl RelonFunction for NoopFn {
        fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }
}
