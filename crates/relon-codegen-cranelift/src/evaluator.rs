//! `AotEvaluator` — the runtime façade for the cranelift
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

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use cranelift_jit::JITModule;

use relon_eval_api::buffer::{BufferBuilder, BufferError, BufferReader};
use relon_eval_api::inplace_return::{
    decode_inplace_list_list_return, decode_inplace_sentinel, ArenaRegions,
};
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::{
    ClosureData, Evaluator, RelonFunction, RuntimeError, Scope, SmolStr, Thunk, Value,
};
use relon_ir::ir::NativeImport;
use relon_parser::{Node, TokenRange};

use crate::cache::CacheEntry;
use crate::codegen::{self, CompiledModule, EntryShape};
use crate::error::CraneliftError;
use crate::object_cache_integration as cache_int;
use crate::sandbox::{CapabilityVtable, SandboxConfig, SandboxShared, SandboxState, TrapKind};

/// Maximum positional arity supported by the legacy `i64` entry
/// shape. Anchored to the four scratch slots in [`LegacyEntryFn`];
/// longer arities surface as `UnsupportedSignature` before the
/// trampoline tries to dispatch.
const MAX_LEGACY_ARITY: usize = 4;

/// Type alias for the raw `extern "C"` entry the JIT produced when
/// the entry shape is [`EntryShape::LegacyI64Args`]. Five i64s cover
/// the v5-β-1 `#main(Int x, Int y, Int z, Int w)` envelope.
type LegacyEntryFn = unsafe extern "C" fn(*const SandboxState, i64, i64, i64, i64) -> i64;

/// Type alias for the raw `extern "C"` entry the JIT produced when
/// the entry shape is [`EntryShape::BufferProtocol`]. The signature
/// mirrors the wasm-AOT side's `run_main`: four i32 buffer-handshake
/// slots plus the i64 capability bitmap, returning the i32
/// `bytes_written`.
type BufferEntryFn = unsafe extern "C" fn(
    *const SandboxState,
    i32, // in_ptr (byte offset inside arena)
    i32, // in_len
    i32, // out_ptr (byte offset inside arena)
    i32, // out_cap
    i64, // caps
) -> i32;

/// Discriminated entry pointer covering both calling-convention
/// shapes. The choice is fixed at compile time by [`EntryShape`].
#[derive(Clone, Copy)]
enum EntryPtr {
    Legacy(LegacyEntryFn),
    Buffer(BufferEntryFn),
}

thread_local! {
    /// Per-thread `Box<SandboxState>` slot reused across `run_main`
    /// dispatches on this thread. The pool elides the per-call
    /// `Box::new` (heap alloc ~30-50 ns on x86_64 glibc) by parking
    /// the previous dispatch's allocation here on the way out and
    /// fishing it back at the top of the next dispatch.
    ///
    /// Stored as `RefCell<Option<Box<_>>>` so a reentrant dispatch
    /// (host helper calling back into `run_main` on the same thread)
    /// sees an empty slot and falls back to a fresh `Box::new`. The
    /// inner call then races to its own freshly-allocated state; the
    /// outer call still owns the slot we tried to borrow. The
    /// `RefCell` itself is never contended across threads because
    /// `thread_local!` gives each thread its own copy.
    static SANDBOX_STATE_POOL: std::cell::RefCell<Option<Box<SandboxState>>> =
        const { std::cell::RefCell::new(None) };
}

/// RAII handle owning the dispatch-call's `SandboxState`. On drop,
/// attempts to park the boxed state back into the thread-local pool
/// so the next dispatch on this thread can skip the `Box::new`.
///
/// Reentrant dispatches on the same thread (host helper calling back
/// into `run_main`) are naturally handled: the inner call observes
/// an empty pool slot (because the outer call already took the box),
/// allocates fresh, and parks on drop. The outer call's drop then
/// sees the slot already filled by the inner call and silently lets
/// its own box drop — net pool size stays at one box per thread.
struct PooledSandboxState {
    state: Option<Box<SandboxState>>,
}

impl PooledSandboxState {
    /// Acquire a `SandboxState` configured against `template`. Prefers
    /// the thread-local pool; falls back to a fresh `Box::new` when
    /// the slot is empty (cold thread or reentrant call).
    fn acquire(template: &SandboxShared) -> Self {
        let taken =
            SANDBOX_STATE_POOL.with(|slot| slot.try_borrow_mut().ok().and_then(|mut b| b.take()));
        let boxed = match taken {
            Some(mut existing) => {
                existing.refresh_from_template(template);
                existing
            }
            None => Box::new(SandboxState::from_template(template)),
        };
        Self { state: Some(boxed) }
    }

    fn state(&self) -> &SandboxState {
        // Always populated until Drop runs.
        self.state.as_deref().expect("sandbox state taken twice")
    }
}

impl Drop for PooledSandboxState {
    fn drop(&mut self) {
        let Some(boxed) = self.state.take() else {
            return;
        };
        SANDBOX_STATE_POOL.with(|slot| {
            if let Ok(mut borrow) = slot.try_borrow_mut() {
                if borrow.is_none() {
                    *borrow = Some(boxed);
                }
            }
        });
    }
}

/// Optional schema metadata kept alive for buffer-protocol modules
/// — populated from `lower_workspace_single` via [`Self::from_source`].
#[derive(Clone)]
struct BufferSchema {
    main_schema: Schema,
    return_schema: Schema,
    main_layout: OffsetTable,
    return_layout: OffsetTable,
}

/// Backing storage that keeps the entry's machine code mapped for the
/// evaluator's lifetime. v5-γ stage 2 unifies the in-process JIT path
/// with the dlopen'd ET_DYN path under a single enum so the rest of
/// the runtime stays backend-agnostic.
#[allow(dead_code, clippy::large_enum_variant)] // each variant owns
                                                // a resource kept alive purely for `Drop`; the runtime never reads
                                                // the payload. `Box<JITModule>` would lose the JIT path's stable
                                                // data pointer for `get_finalized_data`, which we rely on for the
                                                // vtable populate step.
enum EntryBacking {
    /// Live JIT module the codegen pass finalized. We never tear this
    /// down at run time; one module per evaluator is enough.
    Jit(JITModule),
    /// `relon-object-cache` loader handle holding the dlopen'd ET_DYN
    /// mmap'd and `dlsym`-resolved. Dropping this releases the
    /// `dlclose` + memfd cleanup.
    Dlopen(relon_object_cache::LoadedObject),
}

// SAFETY: both variants are `Send + Sync` in their own crates. The
// enum carries no shared mutable state of its own.
unsafe impl Send for EntryBacking {}
unsafe impl Sync for EntryBacking {}

/// AOT evaluator backed by a cranelift JIT module or a dlopen'd
/// cached object (v5-γ stage 2).
pub struct AotEvaluator {
    /// Backing storage for the entry's machine code (JIT module or
    /// dlopen'd cache object). Kept alive for the evaluator's
    /// lifetime so the function pointers in `entry_fn` /
    /// `closure_table` stay valid.
    _module: EntryBacking,
    /// 2026-05-21 dispatch-boundary lever (d): de-tagged inline cache
    /// of the legacy-shape entry pointer. Populated at construction
    /// when the JIT produced a `LegacyI64Args`-shaped entry; `None`
    /// for buffer-protocol evaluators. The legacy hot dispatch path
    /// reads this field directly with one load instead of matching on
    /// an `EntryPtr` enum discriminant on every invoke.
    legacy_entry_cached: Option<LegacyEntryFn>,
    /// 2026-05-21 dispatch-boundary lever (d): symmetric inline cache
    /// for the buffer-protocol entry pointer. See `legacy_entry_cached`.
    buffer_entry_cached: Option<BufferEntryFn>,
    /// Number of declared `#main` parameters. For the buffer-protocol
    /// shape this counts user fields (matching
    /// `main_schema.fields.len()`); for the legacy shape it counts
    /// I64 args directly. Hosts use this as a fast arity sanity check.
    entry_arity: usize,
    /// Parameter names in declaration order. For buffer-protocol
    /// modules these come from `main_schema.fields`; for legacy
    /// modules they're synthetic `arg0`/`arg1`/...
    param_names: Vec<String>,
    /// Source range of the entry `#main` directive.
    entry_range: TokenRange,
    /// 2026-05-22 P0 fix: immutable sandbox template the evaluator
    /// owns between dispatches. Each `run_main` allocates its own
    /// per-call `SandboxState` from this snapshot so concurrent
    /// invocations from multiple threads never share the
    /// `UnsafeCell<_>` arena / cursor fields. See
    /// [`SandboxState`]'s top-comment for the unsafety history.
    ///
    /// Holds the live deadline + capability vtable + closure-table
    /// base; updates flow through `set_deadline` /
    /// `install_capabilities_mut` and only become visible on the
    /// **next** dispatch (the current dispatch already snapshotted at
    /// the top of `from_template`).
    sandbox_shared: Arc<SandboxShared>,
    /// Schema descriptors for the buffer-protocol path. `None` for
    /// legacy direct-IR modules.
    buffer_schema: Option<BufferSchema>,
    /// Const-data bytes (string / list literals) the entry references
    /// via fixed arena-relative offsets. Empty when the entry uses no
    /// constants. Copied into the arena prefix at each call so the
    /// cranelift code can dereference `ConstString { idx }` offsets
    /// directly without a runtime lookup.
    const_data: Vec<u8>,
    /// Stage 5 Phase C.4: closure fn-pointer table. One `usize` per
    /// `IrModule::closure_table` entry; populated after JIT finalize
    /// by resolving each lambda `FuncId` through
    /// `JITModule::get_finalized_function`. The sandbox template's
    /// `closure_table_base` is installed to point at this vec's
    /// element zero so `Op::CallClosure` can dereference the slot.
    ///
    /// Wrapped in `Box<[usize]>` so the address is stable for the
    /// evaluator's lifetime (we install a raw pointer into the
    /// sandbox template). Empty when the module has no lambdas. The
    /// field is never directly read post-construction — keeping it
    /// alive is the entire point, so the JIT-side `Op::CallClosure`
    /// indirect address resolution doesn't dangle.
    #[allow(dead_code)]
    closure_table: Box<[usize]>,
    /// `#native` imports the lowering pass interned for this module, in
    /// `import_idx` order. Carried so [`Self::with_host_fns`] can match a
    /// host-supplied `Arc<dyn RelonFunction>` to the `import_idx` the
    /// source-lowered `Op::CallNative` references. Empty for legacy
    /// direct-IR / cache-loaded modules.
    native_imports: Vec<NativeImport>,
}

// SAFETY: The JIT-emitted code is reentrant, and `SandboxShared`'s
// fields are all atomics or `Mutex<Arc<_>>` — neither requires
// exclusive thread access. `JITModule` is `Send + Sync` in
// cranelift's current public surface. Every per-call `SandboxState`
// is freshly boxed inside `run_main`, so the unsafe shared-state
// path the original `unsafe impl Sync for SandboxState` papered over
// (see 2026-05-22 P0 fix) is gone.
unsafe impl Send for AotEvaluator {}
unsafe impl Sync for AotEvaluator {}

impl AotEvaluator {
    /// Drive the full pipeline: parse + analyze + lower + cranelift
    /// codegen + JIT finalize.
    ///
    /// v5-β-2 widens this from a thin stub to the real end-to-end
    /// path. The lowering pass produces a buffer-protocol shaped IR
    /// (`#main` signature = `(I32, I32, I32, I32, I64) -> I32`) plus
    /// the canonical main / return schemas; both are captured on the
    /// evaluator so `run_main` can serialise / deserialise the
    /// buffers through the same `BufferBuilder` / `BufferReader`
    /// helpers the wasm-AOT backend uses.
    pub fn from_source(src: &str) -> Result<Self, CraneliftError> {
        Self::from_source_with_options(src, &relon_analyzer::AnalyzeOptions::default())
    }

    /// Like [`Self::from_source`] but with caller-supplied analyzer
    /// options — the entry point for host-registered native fns. The
    /// host populates `options.host_fn_names` / `host_fn_signatures` /
    /// `host_fn_gates` / `caps` so the analyzer resolves the calls,
    /// runs the single-file capability-reachability check (a gated call
    /// without the granted cap fails the build here), and the lowering
    /// pass emits the `Op::CheckCap`-guarded `Op::CallNative`.
    ///
    /// The capability *guard* is enforced end-to-end: a `CheckCap`
    /// against an unregistered cap slot traps `CapabilityDenied` at
    /// runtime. Full host-fn *dispatch* on this backend additionally
    /// needs an `Arc<dyn RelonFunction>` → `extern "C"` thunk and a
    /// vtable that separates the import-slot namespace from the
    /// cap-bit namespace — tracked as a follow-up; see the
    /// capability/trust model doc §9.2.
    pub fn from_source_with_options(
        src: &str,
        options: &relon_analyzer::AnalyzeOptions,
    ) -> Result<Self, CraneliftError> {
        let (ir_module, main_schema, return_schema) =
            Self::lower_source_with_options(src, options)?;
        let main_layout = SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| CraneliftError::Lowering(format!("main schema layout: {e}")))?;
        let return_layout = SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| CraneliftError::Lowering(format!("return schema layout: {e}")))?;
        let param_names: Vec<String> = main_schema.fields.iter().map(|f| f.name.clone()).collect();
        let buffer_schema = BufferSchema {
            main_schema,
            return_schema,
            main_layout,
            return_layout,
        };
        let sandbox_cfg = SandboxConfig::default();
        Self::from_ir_inner(ir_module, sandbox_cfg, param_names, Some(buffer_schema))
    }

    /// Skip parse + analyze + lower; rebuild a JIT module from the
    /// cached IR. Slower than a true binary cache (we still re-JIT)
    /// but already much faster than `from_source` because parse +
    /// analyze + lower commonly dominate cold-start.
    ///
    /// Today the cache only ever holds legacy-shape modules; buffer-
    /// protocol caches arrive alongside the schema serialisation
    /// work scheduled for stage 3.
    pub fn from_cache(entry: CacheEntry) -> Result<Self, CraneliftError> {
        let arity = ir_param_count(&entry.ir)?;
        Self::from_ir_inner(
            entry.ir,
            entry.sandbox,
            default_param_names_for(arity),
            None,
        )
    }

    /// v5-γ: drive the full `from_source` pipeline and, as a
    /// side-effect, persist a cache pair (object-cache ET_DYN + IR
    /// cache) under `cache_dir` so subsequent cold starts can skip
    /// parse + analyze + lower. The in-mem JIT still runs this call
    /// — the cache write feeds *next* cold start.
    ///
    /// Cache write is best-effort: any I/O / linker / HMAC failure
    /// downgrades to a logged warning (see
    /// [`crate::object_cache_integration`]) without affecting the
    /// returned evaluator. Hosts can pass `default_cache_dir()` from
    /// the same module for the conventional `$XDG_CACHE_HOME/relon`
    /// path.
    pub fn from_source_with_cache(source: &str, cache_dir: &Path) -> Result<Self, CraneliftError> {
        // 1. Standard `from_source` pipeline — this is what answers
        // the live invocation.
        let (ir_module, main_schema, return_schema) = Self::lower_source(source)?;
        let main_layout = SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| CraneliftError::Lowering(format!("main schema layout: {e}")))?;
        let return_layout = SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| CraneliftError::Lowering(format!("return schema layout: {e}")))?;
        let param_names: Vec<String> = main_schema.fields.iter().map(|f| f.name.clone()).collect();
        let buffer_schema = BufferSchema {
            main_schema: main_schema.clone(),
            return_schema: return_schema.clone(),
            main_layout,
            return_layout,
        };
        let sandbox_cfg = SandboxConfig::default();

        // 2. Persist the cache pair in parallel. We do this *before*
        // building the evaluator so a corrupt-IR panic in the
        // codegen path doesn't leave a stale cache file behind from
        // a stray previous run. The schema cache feeds the dlopen-
        // exec fast-restore path so `from_cache_dir` can skip parse
        // + analyze + lower entirely.
        let source_hash = cache_int::compute_source_hash(source, &sandbox_cfg);
        let return_root_size = buffer_schema.return_layout.root_size as u32;
        Self::write_cache_pair_best_effort(
            source,
            &ir_module,
            &main_schema,
            &return_schema,
            &param_names,
            return_root_size,
            &sandbox_cfg,
            source_hash,
            cache_dir,
        );

        // 3. Build the live JIT-backed evaluator.
        Self::from_ir_inner(ir_module, sandbox_cfg, param_names, Some(buffer_schema))
    }

    /// v5-γ: validate an on-disk cache pair against `source` and
    /// reconstruct an evaluator. Returns `Ok(None)` on a clean miss
    /// (cache absent, integrity failure, metadata mismatch).
    ///
    /// Until the codegen-helper-call vtable indirection lands (see
    /// `object_cache_integration` module docs), the reconstructed
    /// evaluator still drives parse + analyze + lower + JIT in
    /// memory. The cache files are validated end-to-end (HMAC +
    /// integrity + metadata) so a corrupt or mismatched cache is
    /// detected and removed; the production hot path then writes a
    /// fresh pair. The dlopen-execution shortcut activates in a
    /// follow-up phase once cranelift codegen routes
    /// `relon_now` / `relon_raise_trap` / `relon_cap_lookup` through
    /// a `__relon_capability_vtable` indirection.
    pub fn from_cache_dir(source: &str, cache_dir: &Path) -> Result<Option<Self>, CraneliftError> {
        let sandbox_cfg = SandboxConfig::default();
        let source_hash = cache_int::compute_source_hash(source, &sandbox_cfg);

        // Metadata fingerprint: a cache file targeting a different
        // generator / cap_bitmap / signature is invalidated below.
        let expected = Self::expected_metadata_for_source(source, &sandbox_cfg);

        let loaded = match cache_int::try_load_from_cache(cache_dir, source_hash, &expected)? {
            Some(l) => l,
            None => return Ok(None),
        };

        // Cross-check the IR-cache sandbox config against the
        // runtime sandbox; drift invalidates the pair.
        if !sandbox_matches(&loaded.ir_entry.sandbox, &sandbox_cfg) {
            tracing::warn!(
                target: "relon::object_cache",
                "ir-cache sandbox config drift; invalidating pair"
            );
            Self::invalidate_cache_triple(cache_dir, source_hash);
            return Ok(None);
        }

        // v5-γ stage 2: load the schema cache so the trampoline can
        // skip parse + analyze + lower. Without it, the dlopen path
        // would have to re-derive schemas from source on every cold
        // start — blowing the 15 µs strict-mode budget.
        //
        // #171: the sidecar HMAC binds to the just-verified object
        // hash + the source key + the entry-shape/arity it carries.
        // A tampered sidecar surfaces as an hmac-mismatch error so we
        // invalidate the triple fail-closed.
        let schema_path = crate::schema_cache::schema_cache_path_for(cache_dir, source_hash);
        let schema_bytes = match std::fs::read(&schema_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    target: "relon::object_cache",
                    "schema cache miss at {}; falling back to source",
                    schema_path.display()
                );
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return Ok(None);
            }
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "schema cache read failed: {e}"
                );
                return Ok(None);
            }
        };
        let schema_entry = match crate::schema_cache::deserialize(
            &schema_bytes,
            &source_hash,
            &loaded.object_sha256,
            &loaded.hmac_key,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "schema cache decode failed: {e}; invalidating triple"
                );
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return Ok(None);
            }
        };

        // Build the dlopen + dlsym symbol set. We resolve `run_main`
        // + the capability vtable + every closure-table symbol up
        // front so the trampoline can dispatch without a second
        // dlsym round-trip.
        let mut symbols: Vec<String> = Vec::with_capacity(2 + schema_entry.closure_count as usize);
        symbols.push("run_main".to_string());
        symbols.push(crate::vtable::VTABLE_SYMBOL.to_string());
        for i in 0..schema_entry.closure_count {
            symbols.push(format!("__closure_{i}"));
        }
        let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        let loaded_object = match relon_object_cache::LoadedObject::from_bytes(
            &loaded.object_bytes,
            cache_int::host_target_triple(),
            &sym_refs,
        ) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "dlopen of cached object failed: {e}; falling back to source"
                );
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return Ok(None);
            }
        };

        // Resolve the entry + vtable + closure symbol addresses.
        let entry_ptr = match loaded_object.resolve("run_main") {
            Some(p) if !p.is_null() => p,
            _ => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "cached object missing `run_main` symbol; invalidating"
                );
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return Ok(None);
            }
        };
        let vtable_ptr = match loaded_object.resolve(crate::vtable::VTABLE_SYMBOL) {
            Some(p) if !p.is_null() => p,
            _ => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "cached object missing `{}` symbol; invalidating",
                    crate::vtable::VTABLE_SYMBOL
                );
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return Ok(None);
            }
        };

        // v5-γ stage 2: populate the dlopen'd vtable so the
        // host helper pointers are valid before the first call.
        unsafe {
            crate::vtable::populate_vtable(vtable_ptr as *mut u8);
        }

        // Resolve closure-table symbols. The order must match the
        // codegen's deterministic `__closure_<N>` naming so the
        // sandbox state's `closure_table_base` indexes line up.
        let mut closure_table: Vec<usize> = Vec::with_capacity(schema_entry.closure_count as usize);
        for i in 0..schema_entry.closure_count {
            let name = format!("__closure_{i}");
            match loaded_object.resolve(&name) {
                Some(p) => closure_table.push(p as usize),
                None => {
                    tracing::warn!(
                        target: "relon::object_cache",
                        "cached object missing closure symbol `{}`; invalidating",
                        name
                    );
                    Self::invalidate_cache_triple(cache_dir, source_hash);
                    return Ok(None);
                }
            }
        }

        let evaluator =
            Self::from_loaded_object(loaded_object, entry_ptr, schema_entry, closure_table)?;
        Ok(Some(evaluator))
    }

    /// Drop all three cache files (object + IR + schema). Used when
    /// any single file fails integrity / decode so the next cold
    /// start re-emits a consistent triple.
    fn invalidate_cache_triple(cache_dir: &Path, source_hash: [u8; 32]) {
        let _ = std::fs::remove_file(cache_int::ir_cache_path_for(cache_dir, source_hash));
        let _ = std::fs::remove_file(crate::schema_cache::schema_cache_path_for(
            cache_dir,
            source_hash,
        ));
        let _ = std::fs::remove_file(relon_object_cache::storage::cache_path_for(
            cache_dir,
            source_hash,
        ));
    }

    /// v5-γ stage 2: build a `AotEvaluator` whose entry
    /// function lives inside a dlopen'd ET_DYN (rather than a live
    /// JIT module). The caller has already resolved the entry +
    /// vtable + closure-table addresses via `dlsym`; we just wire
    /// them into the runtime structures.
    fn from_loaded_object(
        loaded: relon_object_cache::LoadedObject,
        entry_ptr: *const u8,
        schema_entry: crate::schema_cache::SchemaCacheEntry,
        closure_table_vec: Vec<usize>,
    ) -> Result<Self, CraneliftError> {
        let entry_shape = match schema_entry.entry_shape {
            crate::schema_cache::SerEntryShape::LegacyI64Args => EntryShape::LegacyI64Args,
            crate::schema_cache::SerEntryShape::BufferProtocol => EntryShape::BufferProtocol,
        };
        // SAFETY: the function pointer comes from dlsym on a freshly
        // dlopen'd ET_DYN and remains valid for the lifetime of
        // `loaded`, which we move into the evaluator below.
        let entry_fn = match entry_shape {
            EntryShape::LegacyI64Args => EntryPtr::Legacy(unsafe {
                std::mem::transmute::<*const u8, LegacyEntryFn>(entry_ptr)
            }),
            EntryShape::BufferProtocol => EntryPtr::Buffer(unsafe {
                std::mem::transmute::<*const u8, BufferEntryFn>(entry_ptr)
            }),
        };

        // Build buffer schema metadata when the dlopen'd entry uses
        // the buffer-protocol shape (the default for any source that
        // came through `from_source_with_cache`). Layouts are
        // recomputed from the cached schemas — cheap because schema
        // sizes are bounded.
        let buffer_schema = match entry_shape {
            EntryShape::BufferProtocol => {
                let main_layout = SchemaLayout::offsets_for(&schema_entry.main_schema)
                    .map_err(|e| CraneliftError::Lowering(format!("main layout: {e}")))?;
                let return_layout = SchemaLayout::offsets_for(&schema_entry.return_schema)
                    .map_err(|e| CraneliftError::Lowering(format!("return layout: {e}")))?;
                Some(BufferSchema {
                    main_schema: schema_entry.main_schema.clone(),
                    return_schema: schema_entry.return_schema.clone(),
                    main_layout,
                    return_layout,
                })
            }
            EntryShape::LegacyI64Args => None,
        };

        let entry_range: TokenRange = schema_entry.entry_range.into();
        let closure_table: Box<[usize]> = closure_table_vec.into_boxed_slice();

        let capabilities = Arc::new(CapabilityVtable::with_capacity(64));
        let sandbox_shared = Arc::new(SandboxShared::new(capabilities));
        // SAFETY: closure_table is Box-allocated and lives on the
        // evaluator; the raw pointer stays valid for the evaluator's
        // lifetime.
        let base = if closure_table.is_empty() {
            0
        } else {
            closure_table.as_ptr() as usize
        };
        sandbox_shared.set_closure_table_base(base);

        let arity = buffer_schema
            .as_ref()
            .map(|bs| bs.main_schema.fields.len())
            .unwrap_or(schema_entry.entry_arity as usize);

        let (legacy_entry_cached, buffer_entry_cached) = match entry_fn {
            EntryPtr::Legacy(f) => (Some(f), None),
            EntryPtr::Buffer(f) => (None, Some(f)),
        };
        Ok(Self {
            _module: EntryBacking::Dlopen(loaded),
            legacy_entry_cached,
            buffer_entry_cached,
            entry_arity: arity,
            param_names: schema_entry.param_names,
            entry_range,
            sandbox_shared,
            buffer_schema,
            const_data: schema_entry.const_data,
            closure_table,
            // Cache-loaded modules carry no IR import table; host-fn
            // dispatch from a cached object is out of scope.
            native_imports: Vec::new(),
        })
    }

    /// Compute the expected metadata fingerprint for a (source,
    /// sandbox) pair. Centralised here so `from_source_with_cache`
    /// and `from_cache_dir` agree on what counts as a match.
    fn expected_metadata_for_source(
        source: &str,
        sandbox: &SandboxConfig,
    ) -> relon_object_cache::Metadata {
        // Hash the source bytes into a 32-byte signature stamp so a
        // drift in source text surfaces as a metadata mismatch
        // distinct from the filename-level source-hash drift. We
        // intentionally don't hash sandbox bits into the signature
        // because they already feed into the source-hash filename.
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"relon-main-signature/v1\0");
        hasher.update(source.as_bytes());
        let mut sig = [0u8; 32];
        sig.copy_from_slice(&hasher.finalize());

        // Match the layout used by `write_cache_pair_best_effort` —
        // both call sites must agree byte-for-byte on the trailer
        // contents (minus `created_at_unix`).
        let _ = sandbox;
        cache_int::build_metadata(sandbox, /*cap_bitmap=*/ 0, sig, Vec::new())
    }

    /// Persist the (object-cache, IR-cache, schema-cache) triple for
    /// a successful source build. All failures are swallowed at the
    /// `object_cache_integration` layer — caller does not propagate.
    ///
    /// The schema cache lets `from_cache_dir` skip parse, analyze,
    /// and lower on the next cold start: it round-trips the main and
    /// return schemas plus the entry shape and the closure count so
    /// the dlopen path can wire the trampoline directly.
    #[allow(clippy::too_many_arguments)]
    fn write_cache_pair_best_effort(
        source: &str,
        ir_module: &relon_ir::ir::Module,
        main_schema: &Schema,
        return_schema: &Schema,
        param_names: &[String],
        return_root_size: u32,
        sandbox: &SandboxConfig,
        source_hash: [u8; 32],
        cache_dir: &Path,
    ) {
        // 1. Serialize IR for the fast-restore half. Failure here
        // skips both halves so the cache stays consistent.
        let entry = crate::cache::CacheEntry {
            ir: ir_module.clone(),
            sandbox: sandbox.clone(),
        };
        let ir_bytes = match crate::cache::serialize(&entry) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "ir-cache serialise failed: {e}; skipping cache write"
                );
                return;
            }
        };

        // 2. Emit ET_REL bytes via cranelift-object for the dlopen-
        // load half. v5-γ stage 2 lowers the **full** module (not the
        // stage 1 stub) so the dlopen path can execute real compiled
        // code. Failure here skips just the object cache; the IR
        // cache still goes through so the fast-restore path remains
        // useful on next cold start.
        let metadata = Self::expected_metadata_for_source(source, sandbox);
        let artifact =
            match cache_int::emit_module_object_bytes(ir_module, sandbox, return_root_size) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(
                        target: "relon::object_cache",
                        "cranelift-object emit failed: {e}; skipping object cache"
                    );
                    Self::write_ir_only(cache_dir, source_hash, &ir_bytes);
                    return;
                }
            };

        // 3. Build the schema entry; we'll only HMAC-seal + persist
        // it after the object cache lands so we know the
        // `object_sha256` to bind to.
        let schema_entry = crate::schema_cache::SchemaCacheEntry {
            main_schema: main_schema.clone(),
            return_schema: return_schema.clone(),
            param_names: param_names.to_vec(),
            const_data: artifact.const_data.clone(),
            closure_count: artifact.closure_symbols.len() as u32,
            entry_shape: match artifact.entry_shape {
                crate::codegen::EntryShape::LegacyI64Args => {
                    crate::schema_cache::SerEntryShape::LegacyI64Args
                }
                crate::codegen::EntryShape::BufferProtocol => {
                    crate::schema_cache::SerEntryShape::BufferProtocol
                }
            },
            entry_arity: artifact.entry_arity as u32,
            entry_range: artifact.entry_range.into(),
        };

        // 4. Persist object + IR. `try_store_to_cache` surfaces the
        // linked ET_DYN's SHA-256 only on a fully successful pair
        // write; on best-effort skip (no HMAC key, no linker, etc.)
        // we drop the schema cache too so the triple stays consistent.
        let stored = match cache_int::try_store_to_cache(
            cache_dir,
            source_hash,
            &artifact.et_rel_bytes,
            &metadata,
            &ir_bytes,
        ) {
            Ok(Some(s)) => s,
            Ok(None) => {
                // Object + IR write skipped. Schema cache would be
                // useless without them; bail without writing it.
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "cache write returned unexpected error: {e}"
                );
                return;
            }
        };

        // 5. HMAC-seal the schema sidecar binding it to the source
        // key + the just-written object hash + the entry shape/arity.
        // Resolving the key here a second time keeps the schema layer
        // independent of the object-cache layer's internal handle; on
        // the typical hot path it's a memoised file read.
        let hmac_key = match relon_object_cache::ensure_key() {
            Ok(k) => k,
            Err(e) => {
                // Object cache wrote successfully above (which means
                // HMAC was available there). Hitting an error here is
                // unexpected — surface at warn so operators see it,
                // and roll back the freshly-written object/IR pair so
                // the next cold start regenerates a consistent triple
                // with a working key.
                tracing::warn!(
                    target: "relon::object_cache",
                    "schema cache HMAC key disappeared mid-write ({e}); \
                     invalidating freshly-written cache triple"
                );
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return;
            }
        };
        let schema_bytes = match crate::schema_cache::serialize(
            &schema_entry,
            &source_hash,
            &stored.object_sha256,
            &hmac_key,
        ) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "schema cache serialise failed: {e}; invalidating triple"
                );
                // Without a sidecar the next `from_cache_dir` would
                // refuse the pair anyway; clean up so we re-emit a
                // valid triple on the next cold start.
                Self::invalidate_cache_triple(cache_dir, source_hash);
                return;
            }
        };

        // Schema cache write is best-effort, but its absence forces a
        // fallback so we surface failures at `info`.
        let schema_path = crate::schema_cache::schema_cache_path_for(cache_dir, source_hash);
        if Self::atomic_cache_write("schema cache", cache_dir, &schema_path, &schema_bytes) {
            tracing::debug!(
                target: "relon::object_cache",
                "schema cache wrote {} bytes to {}",
                schema_bytes.len(),
                schema_path.display()
            );
        }
    }

    /// Best-effort atomic write of `bytes` to `dest`: `create_dir_all`
    /// the parent `cache_dir`, write to a per-process `tmp.{pid}.{nanos}`
    /// sidecar, then `rename` into place so a concurrent reader never
    /// observes a torn file. On any failure the sidecar is removed and a
    /// `warn!` is emitted prefixed with `label`. Returns `true` iff the
    /// rename landed.
    fn atomic_cache_write(label: &str, cache_dir: &Path, dest: &Path, bytes: &[u8]) -> bool {
        if let Err(e) = std::fs::create_dir_all(cache_dir) {
            tracing::warn!(
                target: "relon::object_cache",
                "{label} create_dir_all failed: {e}"
            );
            return false;
        }
        let tmp = dest.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            tracing::warn!(
                target: "relon::object_cache",
                "{label} tmp write failed: {e}"
            );
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, dest) {
            tracing::warn!(
                target: "relon::object_cache",
                "{label} rename failed: {e}"
            );
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }

    /// Fallback path: persist just the IR-cache half when the
    /// cranelift-object emit step fails. Same atomic-rename
    /// behaviour as the integration layer.
    fn write_ir_only(cache_dir: &Path, source_hash: [u8; 32], ir_bytes: &[u8]) {
        let path = cache_int::ir_cache_path_for(cache_dir, source_hash);
        Self::atomic_cache_write("ir-cache-only", cache_dir, &path, ir_bytes);
    }

    /// Internal helper: lower a source string into an IR module and
    /// surface the canonical main / return schemas the buffer
    /// protocol talks against. Mirrors
    /// `relon_codegen_wasm::WasmAotEvaluator::compile_source`.
    fn lower_source(src: &str) -> Result<(relon_ir::ir::Module, Schema, Schema), CraneliftError> {
        Self::lower_source_with_options(src, &relon_analyzer::AnalyzeOptions::default())
    }

    fn lower_source_with_options(
        src: &str,
        options: &relon_analyzer::AnalyzeOptions,
    ) -> Result<(relon_ir::ir::Module, Schema, Schema), CraneliftError> {
        // Compiled backend = standalone analyze (no workspace pass), so
        // force the single-file capability-reachability check on.
        let options = relon_analyzer::AnalyzeOptions {
            standalone_capability_check: true,
            ..options.clone()
        };
        let lowered = relon_ir::frontend::compile(src, &options)?;
        Ok((lowered.module, lowered.main_schema, lowered.return_schema))
    }

    /// Compile from a pre-lowered IR module. Public for direct-IR
    /// callers (tests / benchmarks); the buffer-protocol metadata
    /// path is internal.
    pub fn from_ir_direct(
        ir: relon_ir::ir::Module,
        sandbox_cfg: SandboxConfig,
        param_names: Vec<String>,
    ) -> Result<Self, CraneliftError> {
        Self::from_ir_inner(ir, sandbox_cfg, param_names, None)
    }

    /// Compile from a pre-lowered IR module, optionally annotated
    /// with the buffer-protocol schemas. `buffer_schema = Some(_)`
    /// when the source went through `lower_workspace_single` (the
    /// `from_source` path); `None` for legacy direct-IR construction.
    fn from_ir_inner(
        ir: relon_ir::ir::Module,
        sandbox_cfg: SandboxConfig,
        param_names: Vec<String>,
        buffer_schema: Option<BufferSchema>,
    ) -> Result<Self, CraneliftError> {
        // Snapshot the native-import table before the module is consumed
        // by codegen so `with_host_fns` can map fn names → import_idx.
        let native_imports = ir.imports.clone();
        // The codegen's tail-cursor protocol needs the return record's
        // fixed-area size up front to seed the cursor past the root.
        // Direct-IR / legacy callers without schema metadata pass 0;
        // their bodies don't emit tail records.
        let return_root_size = buffer_schema
            .as_ref()
            .map(|bs| bs.return_layout.root_size as u32)
            .unwrap_or(0);
        let compiled = codegen::compile_module_with(&ir, &sandbox_cfg, return_root_size)?;
        let CompiledModule {
            module,
            entry_fn_id,
            entry_arity,
            entry_range,
            entry_shape,
            const_data,
            closure_func_ids,
            vtable_data_id,
        } = compiled;

        // v5-γ stage 2: populate the JIT-resolved capability vtable.
        // `JITModule::get_finalized_data` returns the live pointer +
        // length the codegen reserved via `declare_vtable_data`; we
        // write the host helper fn pointers into the slots before any
        // entry call runs so the emitted `call_indirect`s land on
        // valid targets.
        unsafe {
            let (ptr, len) = module.get_finalized_data(vtable_data_id);
            debug_assert!(
                len >= crate::vtable::VTABLE_BYTES,
                "JIT vtable data section is too small: {len} < {}",
                crate::vtable::VTABLE_BYTES
            );
            crate::vtable::populate_vtable(ptr as *mut u8);
        }

        // Cross-check: buffer schema metadata must agree with the IR
        // shape the codegen picked. A mismatch here is a programming
        // error in `from_source` rather than a user-visible
        // condition.
        match (entry_shape, &buffer_schema) {
            (EntryShape::BufferProtocol, None) => {
                // The legacy path doesn't speak buffer protocol;
                // surface as Codegen because the only way to land
                // here is to hand the buffer-shape IR into
                // `from_ir_direct`, which isn't a public guarantee
                // today. We accept it and let `run_main` reject the
                // call at dispatch time.
            }
            (EntryShape::LegacyI64Args, Some(_)) => {
                return Err(CraneliftError::Codegen(
                    "buffer-protocol schema metadata supplied with legacy-i64 entry shape".into(),
                ));
            }
            _ => {}
        }

        let raw_ptr = module.get_finalized_function(entry_fn_id);
        // SAFETY: JIT-finalized function pointers are stable for the
        // module's lifetime; we keep the module alive on `Self`.
        // Picking the right `EntryPtr` variant is gated on the
        // compiler's `entry_shape`, which is the source of truth.
        let entry_fn = match entry_shape {
            EntryShape::LegacyI64Args => EntryPtr::Legacy(unsafe {
                std::mem::transmute::<*const u8, LegacyEntryFn>(raw_ptr)
            }),
            EntryShape::BufferProtocol => EntryPtr::Buffer(unsafe {
                std::mem::transmute::<*const u8, BufferEntryFn>(raw_ptr)
            }),
        };

        // Stage 5 Phase C.4: resolve each closure fn id to its host
        // address. We keep the resulting Box<[usize]> alive on the
        // evaluator so the sandbox state's closure_table_base pointer
        // stays valid. The slot order matches `IrModule::closure_table`
        // — fn_table_idx `i` resolves to `closure_table[i]`.
        let closure_table: Box<[usize]> = closure_func_ids
            .iter()
            .map(|fid| module.get_finalized_function(*fid) as usize)
            .collect();

        // 64 slots cover every declared CapabilityBit with headroom;
        // hosts that register a higher cap_bit cause `register` to grow
        // the vector.
        let capabilities = Arc::new(CapabilityVtable::with_capacity(64));
        let sandbox_shared = Arc::new(SandboxShared::new(capabilities));
        // closure_table is Box-allocated and lives on the evaluator;
        // the raw pointer stays valid for the evaluator's lifetime.
        // When the table is empty we install 0 (cranelift never reads
        // through it because no Op::CallClosure was emitted).
        let base = if closure_table.is_empty() {
            0
        } else {
            closure_table.as_ptr() as usize
        };
        sandbox_shared.set_closure_table_base(base);

        // Buffer-protocol arity equals the user-field count when we
        // have a schema; fall back to the IR-param count for legacy
        // / orphaned buffer modules.
        let arity = buffer_schema
            .as_ref()
            .map(|bs| bs.main_schema.fields.len())
            .unwrap_or(entry_arity);

        let (legacy_entry_cached, buffer_entry_cached) = match entry_fn {
            EntryPtr::Legacy(f) => (Some(f), None),
            EntryPtr::Buffer(f) => (None, Some(f)),
        };
        Ok(Self {
            _module: EntryBacking::Jit(module),
            legacy_entry_cached,
            buffer_entry_cached,
            entry_arity: arity,
            param_names,
            entry_range,
            sandbox_shared,
            buffer_schema,
            const_data,
            closure_table,
            native_imports,
        })
    }

    /// Replace the capability vtable wholesale.
    ///
    /// 2026-05-22 P0 fix: previously this rebuilt a brand-new
    /// `Arc<SandboxState>` so the in-flight invocation kept its old
    /// vtable while the next invocation picked up the new one. The
    /// per-call ownership model gives us the same property "for free":
    /// each `run_main` snapshots the template at the top of dispatch,
    /// so swapping the `Arc<CapabilityVtable>` inside the template
    /// here only affects subsequent dispatches.
    pub fn install_capabilities_mut(&mut self, capabilities: Arc<CapabilityVtable>) {
        self.sandbox_shared.set_capabilities(capabilities);
    }

    /// The `#native` imports the lowering pass interned for this
    /// module, in `import_idx` order. Lets a host map fn names to the
    /// slots [`Self::with_host_fns`] fills.
    pub fn native_imports(&self) -> &[NativeImport] {
        &self.native_imports
    }

    /// Register the host's `Arc<dyn RelonFunction>` callables for
    /// source-lowered native-fn dispatch. Each entry is keyed by the
    /// source-level fn name; this matches the name to the `import_idx`
    /// the lowering pass assigned (via [`Self::native_imports`]) and
    /// installs the callable in the capability vtable's `import_idx`-
    /// keyed registry. A source-lowered `Op::CallNative` then dispatches
    /// to it through the `relon_call_native` helper. Names with no
    /// matching `#native` import are skipped.
    ///
    /// The capability *guard* is enforced independently by the
    /// `Op::CheckCap` prologue against the grant set via
    /// [`Self::with_granted_cap`] — registering a callable does not
    /// grant its capability.
    pub fn with_host_fns(self, host_fns: &HashMap<String, Arc<dyn RelonFunction>>) -> Self {
        let mut vt = (*self.sandbox_shared.capabilities_snapshot()).clone();
        for (idx, imp) in self.native_imports.iter().enumerate() {
            if let Some(func) = host_fns.get(&imp.name) {
                vt.register_host_fn(idx as u32, Arc::clone(func));
            }
        }
        self.sandbox_shared.set_capabilities(Arc::new(vt));
        self
    }

    /// Grant a capability bit so the source-lowered `Op::CheckCap`
    /// prologue passes at runtime. Parks a non-null sentinel at the
    /// bit's vtable slot; the actual dispatch goes through the
    /// `import_idx`-keyed host-fn registry. Decoupled from the
    /// analyze-time `caps`: a host can grant statically (build passes
    /// the reachability check) yet withhold here to exercise a stricter
    /// runtime posture (the call then traps `CapabilityDenied`).
    pub fn with_granted_cap(self, bit: u32) -> Self {
        let mut vt = (*self.sandbox_shared.capabilities_snapshot()).clone();
        vt.grant(bit);
        self.sandbox_shared.set_capabilities(Arc::new(vt));
        self
    }

    /// Configure the per-call wall-clock deadline. Pass
    /// `std::time::Duration::MAX` (or any value that overflows the
    /// nanos-as-i64 budget) to disable. The new value applies from the
    /// next dispatch onward — invocations already in flight keep the
    /// deadline they snapshotted at the top of dispatch.
    pub fn set_deadline(&self, deadline: std::time::Duration) {
        self.sandbox_shared.set_deadline(deadline);
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

    /// Fast-path entry for legacy-shape modules: skip the
    /// `HashMap<String, Value>` packing / lookup that `run_main`
    /// performs and call the JIT entry directly with the supplied i64
    /// argument vector.
    ///
    /// Returns `Err(Unsupported)` when the evaluator was built from a
    /// buffer-protocol source (i.e. `from_source` rather than
    /// `from_ir_direct` / `from_cache`). Callers that need a typed
    /// signature contract for both shapes should keep using
    /// [`Evaluator::run_main`].
    ///
    /// Boundary cost vs `run_main`: at the time this fast path was
    /// added (2026-05-21), profiling on `dispatch_cranelift_step`
    /// attributed roughly 200-250 ns / invoke to the HashMap arg
    /// packing + name-keyed lookup that `run_main` does for the legacy
    /// shape. Callers driving a hot loop of Rust→AOT invocations (e.g.
    /// per-record evaluation in a streaming pipeline) should prefer
    /// this entry; per-invoke cost drops into the 150-200 ns band.
    pub fn run_main_legacy_i64(&self, args: &[i64]) -> Result<i64, RuntimeError> {
        self.check_legacy_entry_shape(args.len(), "run_main_legacy_i64")?;
        let mut argv = [0i64; MAX_LEGACY_ARITY];
        argv[..args.len()].copy_from_slice(args);
        self.invoke_legacy_entry(argv)
    }

    /// Shared pre-flight for every legacy `#main(...)` entry: reject
    /// buffer-protocol evaluators, mismatched arity, or arity beyond
    /// [`MAX_LEGACY_ARITY`]. `entry_name` is woven into the
    /// buffer-protocol diagnostic so callers can tell which API path
    /// surfaced the mismatch.
    fn check_legacy_entry_shape(&self, n: usize, entry_name: &str) -> Result<(), RuntimeError> {
        if self.buffer_schema.is_some() {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native: {entry_name} requires a legacy-shape entry; the evaluator was built from buffer-protocol source"
                ),
            });
        }
        if n != self.entry_arity {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native: #main expects {} arg(s), got {}",
                    self.entry_arity, n
                ),
            });
        }
        if n > MAX_LEGACY_ARITY {
            return Err(RuntimeError::Unsupported {
                reason: format!("cranelift-native legacy entry supports up to 4 args; got {n}"),
            });
        }
        Ok(())
    }

    /// Walk `param_names` and use `lookup` to materialise each slot
    /// of the legacy `[i64; 4]` argv. Shared by `run_main_smallmap`
    /// (slice-keyed lookup) and `Evaluator::run_main` (HashMap-keyed
    /// lookup). The lookup closure receives the slot index plus the
    /// declared parameter name and returns either the resolved
    /// `Value` or a missing-arg error.
    fn pack_legacy_argv_by_name<F>(
        &self,
        mut lookup: F,
    ) -> Result<[i64; MAX_LEGACY_ARITY], RuntimeError>
    where
        F: FnMut(usize, &str) -> Result<Value, RuntimeError>,
    {
        let mut argv = [0i64; MAX_LEGACY_ARITY];
        for (i, name) in self.param_names.iter().enumerate() {
            match lookup(i, name)? {
                Value::Int(v) => argv[i] = v,
                other => {
                    return Err(RuntimeError::MainArgTypeMismatch {
                        name: name.clone(),
                        expected: "Int".to_string(),
                        found: other.type_name().to_string(),
                        range: self.entry_range,
                    });
                }
            }
        }
        Ok(argv)
    }

    /// 2026-05-21 dispatch-boundary lever (a): name-keyed entry that
    /// avoids the `HashMap<String, Value>` heap allocation entirely.
    ///
    /// Hosts that already know the argument names statically (the
    /// common case for a hot per-record dispatch loop) can pass a
    /// stack-allocated `&[(&str, Value)]` slice instead of materialising
    /// a `HashMap`. Internally we linearly scan `param_names` against
    /// the supplied slice — for the legacy `#main(Int...)` envelope the
    /// slice is capped at 4 entries, so the scan is faster than a hash
    /// lookup and the heap allocation is gone.
    ///
    /// Returns `Err(Unsupported)` when the evaluator was built from a
    /// buffer-protocol source. Hosts that need full `Evaluator` trait
    /// compatibility should keep using [`Evaluator::run_main`]; this
    /// fast path is for callers who can express their arguments with a
    /// flat slice and want to skip the dict allocation per invoke.
    ///
    /// Boundary cost vs `run_main`: the HashMap path measures roughly
    /// 366 ns / invoke on the `dispatch_cranelift_step` bench row at
    /// the time this lever landed; the SmallMap path drops into the
    /// 230 ns band (saving ~130 ns, dominated by avoiding two `String`
    /// heap allocations plus the `HashMap` bucket allocation + drop).
    pub fn run_main_smallmap(&self, args: &[(&str, Value)]) -> Result<Value, RuntimeError> {
        self.check_legacy_entry_shape(args.len(), "run_main_smallmap")?;
        let argv = self.pack_legacy_argv_by_name(|i, name| {
            // Linear scan: at most 4 entries, faster than a hash lookup
            // for tiny n. Accept either the declared param name or the
            // synthetic `arg{i}` form for hosts that haven't surfaced
            // names yet.
            for (k, v) in args {
                if *k == name {
                    return Ok(v.clone());
                }
            }
            let synth = format!("arg{i}");
            for (k, v) in args {
                if *k == synth.as_str() {
                    return Ok(v.clone());
                }
            }
            Err(RuntimeError::MissingMainArg {
                name: name.to_string(),
                range: self.entry_range,
            })
        })?;
        let result_i64 = self.invoke_legacy_entry(argv)?;
        Ok(Value::Int(result_i64))
    }

    /// Internal: invoke the legacy-shape JIT entry with the supplied
    /// i64 args.
    ///
    /// 2026-05-22 review #172: the `catch_unwind` shield runs in
    /// **both** debug and release builds. The 2026-05-21 lever (b)
    /// release-only skip is reverted because:
    ///
    /// - The shield's role is to convert a Rust panic from a
    ///   misbehaving helper symbol into a typed `RuntimeError`. The
    ///   helper-call surface is audited as non-panicking, but the
    ///   audit is a static promise; debug-only enforcement turned
    ///   the shield into a development-time-only safety net. Release
    ///   builds were one helper regression away from aborting the
    ///   host process.
    /// - The shield is **not** what types SIGSEGV / SIGFPE / SIGILL.
    ///   Those remain fail-fast process crashes unless a future host
    ///   installs a real `sigsetjmp`/landing-pad recovery trampoline.
    ///   The old Rust TLS signal-slot telemetry path is intentionally
    ///   disabled because it was not async-signal-safe.
    /// - Measured release-build cost of the shield on
    ///   `dispatch_cranelift_step_legacy_i64` (#154 baseline) is
    ///   ~1.75 ns / +12 % over the unshielded path — acceptable to
    ///   restore the typed-error guarantee for any helper-panic
    ///   regression that escapes the audit.
    fn invoke_legacy_entry(&self, args: [i64; 4]) -> Result<i64, RuntimeError> {
        // 2026-05-22 P0 fix: allocate a fresh per-call SandboxState
        // from the evaluator's immutable template. Two threads
        // dispatching against the same evaluator each see their own
        // boxed state, so the in-place arena / trap_code writes that
        // used to race are now thread-local.
        //
        // P2-10 follow-up: `PooledSandboxState::acquire` looks up the
        // per-thread pool first and only falls back to `Box::new` on a
        // cold thread / reentrant call. The boxed state is parked
        // back into the pool on drop so the next dispatch on this
        // thread skips the heap alloc (~30-50 ns on x86_64 glibc).
        let pooled = PooledSandboxState::acquire(&self.sandbox_shared);
        let state = pooled.state();
        let state_ptr: *const SandboxState = state;
        // De-tagged inline-cached entry pointer (single field load)
        // beats matching on the `entry_fn` enum discriminant each
        // invoke.
        let entry = match self.legacy_entry_cached {
            Some(f) => f,
            None => {
                return Err(RuntimeError::Unsupported {
                    reason: "cranelift-native: invoke_legacy_entry called on buffer-protocol shape"
                        .into(),
                });
            }
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            entry(state_ptr, args[0], args[1], args[2], args[3])
        }));
        self.dispatch_post(state, result)
    }

    /// Same as [`Self::invoke_buffer_entry_with_scratch`] without an
    /// explicit scratch base — defaults to `0`. Kept for direct-IR
    /// tests that don't route through the schema-aware trampoline.
    #[allow(dead_code)]
    fn invoke_buffer_entry(
        &self,
        arena: &mut [u8],
        in_ptr: u32,
        in_len: u32,
        out_ptr: u32,
        out_cap: u32,
        caps: u64,
    ) -> Result<i32, RuntimeError> {
        self.invoke_buffer_entry_with_scratch(
            arena, in_ptr, in_len, out_ptr, out_cap, /*scratch_base=*/ 0, caps,
        )
    }

    /// Internal: invoke the buffer-protocol JIT entry. Caller owns
    /// the arena bytes (already populated with the input buffer);
    /// this helper installs them into the sandbox state, kicks off
    /// the JIT, and reports either the `bytes_written` from `run_main`
    /// or a typed trap.
    #[allow(clippy::too_many_arguments)]
    fn invoke_buffer_entry_with_scratch(
        &self,
        arena: &mut [u8],
        in_ptr: u32,
        in_len: u32,
        out_ptr: u32,
        out_cap: u32,
        scratch_base: u32,
        caps: u64,
    ) -> Result<i32, RuntimeError> {
        // 2026-05-22 P0 fix: allocate a per-call SandboxState from
        // the evaluator's immutable template. See
        // `invoke_legacy_entry` for the rationale; the buffer entry
        // additionally writes the arena pointer / length / scratch
        // base, which previously raced on a shared Arc<SandboxState>.
        //
        // P2-10 follow-up: pooled via `PooledSandboxState` so the
        // boxed state lives across dispatches on the same thread.
        let pooled = PooledSandboxState::acquire(&self.sandbox_shared);
        let state = pooled.state();
        let state_ptr: *const SandboxState = state;
        // SAFETY: `arena` is borrowed mutably here and stays valid
        // through the JIT call's lifetime (`arena` outlives this
        // function). The per-call `state` is the unique owner of the
        // arena / scratch UnsafeCells for the dispatch's duration, so
        // the writes below cannot race with another thread.
        unsafe {
            state.install_arena(arena.as_mut_ptr(), arena.len() as u32);
            state.install_scratch_base(scratch_base);
        }
        // 2026-05-21 dispatch-boundary lever (d): de-tagged inline
        // cache; see `invoke_legacy_entry` for the rationale.
        let entry = match self.buffer_entry_cached {
            Some(f) => f,
            None => {
                return Err(RuntimeError::Unsupported {
                    reason: "cranelift-native: invoke_buffer_entry called on legacy shape".into(),
                });
            }
        };

        // 2026-05-22 review #172: the `catch_unwind` shield runs in
        // both debug and release. The release-only skip introduced
        // by 2026-05-21 lever (b) is reverted because the
        // helper-non-panic audit is a static promise, not an
        // enforced invariant — see `invoke_legacy_entry` for the
        // full rationale.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            entry(
                state_ptr,
                in_ptr as i32,
                in_len as i32,
                out_ptr as i32,
                out_cap as i32,
                caps as i64,
            )
        }));
        self.dispatch_post(state, result)
    }

    /// Post-process a JIT-call result: surface typed traps recorded in
    /// `state.trap_code`, otherwise pass the raw return value through.
    ///
    /// The old best-effort Rust TLS signal observation path is now a
    /// no-op because writing TLS from a synchronous signal handler is
    /// not async-signal-safe. A genuine SIGSEGV/SIGFPE/SIGILL remains
    /// a platform fail-fast crash until a real host recovery
    /// trampoline exists.
    ///
    /// 2026-05-21 lever (c): success path uses `take_trap_code`
    /// (load-then-store-iff-nonzero) so the predictable-not-taken
    /// branch has no atomic store cost. The signal-slot reset is
    /// also only performed when a signal actually fired.
    fn dispatch_post<T>(
        &self,
        state: &SandboxState,
        result: std::thread::Result<T>,
    ) -> Result<T, RuntimeError> {
        match result {
            Ok(v) => {
                let code = state.take_trap_code();
                if code != 0 {
                    return Err(TrapKind::from_code(code as u8).to_runtime_error(self.entry_range));
                }
                Ok(v)
            }
            Err(payload) => {
                let code = state.take_trap_code();
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

    /// Buffer-protocol `run_main`: serialise the args dict into an
    /// arena, invoke the JIT, deserialise the return record.
    fn run_main_buffer(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        let bs = self.buffer_schema.as_ref().expect("checked by caller");

        // 1. Build the input buffer using `BufferBuilder`.
        let mut builder = BufferBuilder::new(&bs.main_layout, &bs.main_schema.fields);
        for field in &bs.main_schema.fields {
            let value = args
                .get(&field.name)
                .ok_or_else(|| RuntimeError::MissingMainArg {
                    name: field.name.clone(),
                    range: self.entry_range,
                })?;
            write_value_into_builder(&mut builder, field, value, &bs.main_schema.name)?;
        }
        let in_bytes = builder.finish();

        // 2. Lay out the arena: [const_data | pad | in_buf | pad |
        // out_buf]. The const-data section holds string / list
        // literals at compile-time-known offsets; cranelift code
        // dereferences them through fixed `iconst(I32, offset)`
        // values. The host copies the bytes verbatim at the prefix.
        let in_len = in_bytes.len() as u32;
        let out_cap_min = bs.return_layout.root_size as u32;
        let out_cap_cushion: u32 = 1024;
        let out_cap = relon_util::align_up(out_cap_min.max(64) + out_cap_cushion, 8);

        let const_data_len = u32::try_from(self.const_data.len()).map_err(|_| {
            RuntimeError::IoError("cranelift const-data section exceeds u32 range".into())
        })?;
        let in_ptr = relon_util::align_up(const_data_len, 8);
        let out_ptr = relon_util::align_up(in_ptr + in_len, 8);
        // Reserve a scratch region past the output buffer for the
        // memory stdlib (`concat` / `substring` / …) and list
        // materialization (`range().map()` / `_list_filter`) to
        // bump-allocate temporary records. The cursor never resets
        // within a dispatch, so a recursive list-materializing kernel
        // (W16 quicksort partitions O(n log n) sublists across its
        // recursion) needs worst-case headroom. 1 MiB matches the LLVM
        // AOT backend's figure (evaluator.rs scratch_size) so both
        // backends compile the same materialize-heavy workloads at the
        // bench's N (W16_N = 1000); 64 KiB previously trapped past
        // ~n=256. The size is fixed for now; later work can pool /
        // size it from the source when usage gets bigger.
        let scratch_size: u32 = 1_048_576; // 1 MiB
        let scratch_base = relon_util::align_up(out_ptr + out_cap, 8);
        let arena_size = scratch_base
            .checked_add(scratch_size)
            .ok_or_else(|| RuntimeError::IoError("cranelift arena size overflow".into()))?;

        // 3. Borrow a thread-local arena buffer from the pool, sized
        // to `arena_size`. Reuses the per-thread allocation across
        // dispatches so the steady-state cost of a `run_main` no
        // longer pays a fresh ~70 KiB `vec![0u8; …]` (alloc + zero)
        // on every call. We only zero the bytes the JIT can read
        // before writing — see `dispatch_with_pooled_arena` for the
        // detailed zero plan.
        //
        // If the pool is already borrowed (e.g., a stdlib helper
        // reentered the evaluator on the same thread) we transparently
        // fall back to a fresh `Vec<u8>`; correctness wins over the
        // pool hit on that vanishingly rare path.
        ARENA_POOL.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => self.dispatch_with_pooled_arena(
                bs,
                &mut buf,
                arena_size as usize,
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                scratch_base,
                &in_bytes,
            ),
            Err(_) => {
                let mut fallback: Vec<u8> = Vec::new();
                self.dispatch_with_pooled_arena(
                    bs,
                    &mut fallback,
                    arena_size as usize,
                    in_ptr,
                    in_len,
                    out_ptr,
                    out_cap,
                    scratch_base,
                    &in_bytes,
                )
            }
        })
    }

    /// Run a single buffer-protocol dispatch against a caller-provided
    /// `arena` `Vec<u8>`. The vector is resized in place to
    /// `arena_size`; reused capacity is preserved, growth pays a
    /// realloc only on the first oversized request. Only the bytes
    /// the JIT (or the decode step) can read before writing are
    /// zeroed:
    ///
    /// * `[0 .. const_data.len())` — overwritten by the const-data
    ///   copy, so no pre-zero required.
    /// * `[const_data.len() .. in_ptr + in_len)` — covers the
    ///   alignment pad between const-data and the input buffer plus
    ///   the input region itself; the input slice is then copied on
    ///   top.
    /// * `[in_ptr + in_len .. out_ptr + out_cap)` — alignment pad +
    ///   output region. The JIT writes the prefix the trampoline
    ///   reads back (`bytes_written`), but `BufferReader` may consume
    ///   up to `return_layout.root_size` bytes, so any trailing slack
    ///   must read as zero.
    /// * `[scratch_base .. arena_size)` — left uninitialised. The
    ///   scratch bump allocator always writes before it reads (see
    ///   `codegen::memory::emit_alloc_scratch`), so previous-dispatch
    ///   bytes don't leak into observable behaviour.
    ///
    /// Skipping the scratch zero is where the 70 KiB → ~6 KiB
    /// per-dispatch zero savings come from (the scratch region is
    /// the dominant 64 KiB slab).
    #[allow(clippy::too_many_arguments)]
    fn dispatch_with_pooled_arena(
        &self,
        bs: &BufferSchema,
        arena: &mut Vec<u8>,
        arena_size: usize,
        in_ptr: u32,
        in_len: u32,
        out_ptr: u32,
        out_cap: u32,
        scratch_base: u32,
        in_bytes: &[u8],
    ) -> Result<Value, RuntimeError> {
        // Ensure the backing storage is at least `arena_size` bytes
        // and zero the prefix the JIT / decoder can observe. We use
        // `resize` (zero-fill new tail) for growth and an explicit
        // `fill(0)` over the observable prefix on reuse.
        if arena.len() < arena_size {
            arena.resize(arena_size, 0);
        }
        let observable_end = (out_ptr as usize) + (out_cap as usize);
        debug_assert!(observable_end <= arena_size);
        debug_assert!(self.const_data.len() <= in_ptr as usize);
        // Zero the const-data → out-buf prefix. The const-data bytes
        // are about to be overwritten in full, so we can skip them;
        // start zeroing at `const_data.len()` and stop at the scratch
        // boundary.
        arena[self.const_data.len()..observable_end].fill(0);
        if !self.const_data.is_empty() {
            arena[..self.const_data.len()].copy_from_slice(&self.const_data);
        }
        arena[in_ptr as usize..in_ptr as usize + in_bytes.len()].copy_from_slice(in_bytes);

        // Hand only the live `arena_size` slice to the JIT so the
        // sandbox's bounds check still sees the layout-correct length
        // even when the underlying `Vec` is larger from a previous
        // dispatch.
        let live_arena = &mut arena[..arena_size];
        let bytes_written = self.invoke_buffer_entry_with_scratch(
            live_arena,
            in_ptr,
            in_len,
            out_ptr,
            out_cap,
            scratch_base,
            /*caps=*/ 0,
        )?;
        // In-place region-walk return ABI (S1): a negative return value
        // is the in-place sentinel `-(root_abs + 1)`. Instead of a value
        // copied into `out_buf`, the machine code reports the
        // arena-absolute offset of the return root (today only a
        // `List<List<scalar>>` value sourced from a `#main` parameter).
        // We rebase it to its source region, run the bounds verifier over
        // the whole reachable graph confined to that region, and only on
        // a clean verify decode the value in place. A verifier failure is
        // a loud error — we never decode an unverified in-place return.
        if bytes_written < 0 {
            let root_abs = decode_inplace_sentinel(bytes_written)?;
            if !is_single_value_wrapper(&bs.return_schema) {
                return Err(RuntimeError::IoError(
                    "cranelift in-place return on a non-single-value return schema".into(),
                ));
            }
            return decode_inplace_list_list_return(
                "cranelift",
                arena,
                ArenaRegions {
                    const_data_len: self.const_data.len(),
                    in_ptr,
                    in_len,
                    out_ptr,
                    out_cap,
                    scratch_base,
                    arena_size,
                },
                root_abs,
                &bs.return_schema.fields[0],
                &bs.return_layout,
                &bs.return_schema.fields,
            );
        }
        let bw = bytes_written as usize;

        // 4. Decode the output region back to a `Value`. We always
        // give the reader at least `return_root_size` bytes — the
        // wasm side does the same.
        let read_len = bw.max(bs.return_layout.root_size);
        let read_end = out_ptr as usize + read_len;
        if read_end > arena_size {
            return Err(RuntimeError::IoError(
                "cranelift arena too small for return decode".into(),
            ));
        }
        let out_bytes = &arena[out_ptr as usize..read_end];

        let reader = BufferReader::new(&bs.return_layout, &bs.return_schema.fields, out_bytes)
            .map_err(buffer_to_runtime_error)?;
        if is_single_value_wrapper(&bs.return_schema) {
            let field = &bs.return_schema.fields[0];
            read_value_from_reader(&reader, field, &bs.return_schema)
        } else {
            let map = read_record_into_map(&reader, &bs.return_schema)?;
            Ok(Value::branded_dict(
                map,
                Some(bs.return_schema.name.clone()),
            ))
        }
    }
}

thread_local! {
    /// Per-thread arena buffer reused across `run_main_buffer`
    /// dispatches. The buffer caches the largest `arena_size` the
    /// thread has ever requested; subsequent dispatches reuse the
    /// allocation, paying only a targeted `fill(0)` over the bytes
    /// the JIT can observe (not the full ~64 KiB scratch slab).
    ///
    /// Thread-local sidesteps the synchronisation cost of a global
    /// pool and matches the rest of the cranelift backend's runtime
    /// state (`RECORDING_REGISTRY`, etc.), which is already
    /// per-thread by design. Each `try_borrow_mut` is a single
    /// boolean flip, so reentrant calls (a stdlib helper looping
    /// back into the evaluator on the same thread) fall through to
    /// a fresh `Vec` and stay correct.
    static ARENA_POOL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
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

/// Pairwise compare two `SandboxConfig`s by every flag field. Helper
/// for `from_cache_dir` so a runtime / cache mismatch on any single
/// flag invalidates the entry deterministically. Centralised here so
/// the caller doesn't have to derive `PartialEq` on `SandboxConfig`
/// (which would pull other behaviour into the public surface).
fn sandbox_matches(a: &SandboxConfig, b: &SandboxConfig) -> bool {
    a.bounds_check == b.bounds_check
        && a.deadline_check == b.deadline_check
        && a.capability_check == b.capability_check
        && a.div_check == b.div_check
}

impl Evaluator for AotEvaluator {
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
        // Buffer-protocol path: schema-driven serialisation through
        // `BufferBuilder` / `BufferReader`. Selected when the source
        // came in via `from_source`.
        if self.buffer_schema.is_some() {
            return self.run_main_buffer(args);
        }
        // Legacy direct-IR path: pack i64 args into the JIT's
        // `extern "C"` slot.
        self.check_legacy_entry_shape(args.len(), "run_main")?;
        let argv = self.pack_legacy_argv_by_name(|i, name| {
            let synth_key = format!("arg{i}");
            args.get(name)
                .or_else(|| args.get(&synth_key))
                .cloned()
                .ok_or_else(|| RuntimeError::MissingMainArg {
                    name: name.to_string(),
                    range: self.entry_range,
                })
        })?;
        let result_i64 = self.invoke_legacy_entry(argv)?;
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

/// Detect the IR-synthesised `Ret { value: T }` wrapper. Matches when
/// the schema's name is exactly [`relon_ir::MAIN_RETURN_SCHEMA_NAME`]
/// and it carries a single field named
/// [`relon_ir::RETURN_VALUE_FIELD_NAME`].
fn is_single_value_wrapper(schema: &Schema) -> bool {
    schema.name == relon_ir::MAIN_RETURN_SCHEMA_NAME
        && schema.fields.len() == 1
        && schema.fields[0].name == relon_ir::RETURN_VALUE_FIELD_NAME
}

/// Convert a [`BufferError`] into a [`RuntimeError::IoError`] with a
/// `cranelift buffer:` prefix so the diagnostic clearly attributes
/// the failure to the AOT backend.
fn buffer_to_runtime_error(e: BufferError) -> RuntimeError {
    RuntimeError::IoError(format!("cranelift buffer: {e}"))
}

/// Write `value` into the matching slot of `builder`. Mirrors
/// `relon_codegen_wasm::write_value_into_builder` but operates on the
/// cranelift backend's own arena — the layout is identical.
fn write_value_into_builder(
    builder: &mut BufferBuilder<'_>,
    field: &Field,
    value: &Value,
    schema_name: &str,
) -> Result<(), RuntimeError> {
    match (&field.ty, value) {
        (TypeRepr::Int, Value::Int(v)) => builder
            .write_int(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Float, Value::Float(v)) => builder
            .write_float(&field.name, v.into_inner())
            .map_err(buffer_to_runtime_error),
        // Accept Int → Float promotion so JSON `1` flows into a Float
        // slot without forcing the caller to spell `1.0`. Mirrors the
        // tree-walker's leniency.
        (TypeRepr::Float, Value::Int(v)) => {
            let f = *v as f64;
            builder
                .write_float(&field.name, f)
                .map_err(buffer_to_runtime_error)
        }
        (TypeRepr::Bool, Value::Bool(v)) => builder
            .write_bool(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Null, Value::Null) => builder
            .write_null(&field.name)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::String, Value::String(s)) => builder
            .write_string(&field.name, s.as_str())
            .map_err(buffer_to_runtime_error),
        // List-typed `#main` arg: serialise the elements into a
        // pointer-indirect tail record (the same `[len][payload]` /
        // pointer-array shape `ConstListInt` / `ConstListString` bake)
        // and back-patch the 4-byte buffer-relative offset slot the
        // JIT's `LoadList*Ptr` reads. Mirrors the LLVM backend's
        // `marshal_list_*_in`. `List<Schema>` stays a loud cap.
        (TypeRepr::List { element }, Value::List(items)) => {
            write_list_arg_into_builder(builder, &field.name, element, items)
        }
        // Schema-typed `#main` arg: a branded `Value::Dict` is written
        // as a sub-record into the parent buffer's tail and the 4-byte
        // buffer-relative offset slot back-patched — exactly the slot
        // the JIT's `LoadSchemaPtr` reads. Mirrors the LLVM backend's
        // `marshal_schema_in`. Nested schema fields recurse through this
        // same arm; `finish_sub_record` relocates the child's pointer
        // slots into the parent's coordinate system.
        (TypeRepr::Schema { schema }, Value::Dict(dict)) => {
            write_schema_arg_into_builder(builder, &field.name, schema, dict)
        }
        _ => Err(RuntimeError::MainArgTypeMismatch {
            name: field.name.clone(),
            expected: format!("{:?}", field.ty),
            found: value.type_name().to_string(),
            range: TokenRange::default(),
        })
        .map_err(|e| {
            // Attach schema context for clearer diagnostics.
            if let RuntimeError::MainArgTypeMismatch {
                name,
                expected,
                found,
                range,
            } = e
            {
                RuntimeError::MainArgTypeMismatch {
                    name: format!("{schema_name}.{name}"),
                    expected,
                    found,
                    range,
                }
            } else {
                e
            }
        }),
    }
}

/// Marshal a schema-typed `#main` arg (a branded `Value::Dict`) into
/// the parent buffer. Opens a detached sub-record builder via
/// [`BufferBuilder::sub_record`], fills it field-by-field through
/// [`write_schema_fields_into_builder`], then commits it with
/// [`BufferBuilder::finish_sub_record`] (which appends the child to the
/// parent's tail, back-patches the offset slot, and relocates the
/// child's own pointer slots). Mirrors the LLVM backend's
/// `marshal_schema_in` so both backends produce byte-identical input
/// buffers for the same args.
fn write_schema_arg_into_builder(
    builder: &mut BufferBuilder<'_>,
    name: &str,
    schema: &Schema,
    dict: &relon_eval_api::ValueDict,
) -> Result<(), RuntimeError> {
    let sub_layout = SchemaLayout::offsets_for(schema).map_err(|e| RuntimeError::Unsupported {
        reason: format!("cranelift-native: schema arg `{name}` layout: {e}"),
    })?;
    let mut child = builder
        .sub_record(name, &sub_layout, &schema.fields)
        .map_err(buffer_to_runtime_error)?;
    write_schema_fields_into_builder(&mut child, schema, dict, name)?;
    builder
        .finish_sub_record(name, child)
        .map_err(buffer_to_runtime_error)
}

/// Marshal a `List<…>` `#main` arg (or schema List field) into the
/// buffer. Dispatches on the canonical element type to the matching
/// `BufferBuilder::write_list_*` writer, each of which appends the
/// pointer-indirect tail record (`[len][payload]` for scalar elements,
/// a `[len][off_0]…` pointer array of `[len][utf8]` String records for
/// `List<String>`) and back-patches the field's buffer-relative offset
/// slot. Element `Value`s are type-checked against the declared element
/// type. `List<Schema>` (and any other element) stays a loud cap.
/// Mirrors the LLVM backend's `marshal_list_*_in`.
fn write_list_arg_into_builder(
    builder: &mut BufferBuilder<'_>,
    name: &str,
    element: &TypeRepr,
    items: &[Value],
) -> Result<(), RuntimeError> {
    let mismatch = |idx: usize, got: &Value, want: &str| RuntimeError::Unsupported {
        reason: format!(
            "cranelift-native: List<{want}> arg `{name}` element #{idx} got {} but expects {want}",
            got.type_name()
        ),
    };
    match element {
        TypeRepr::Int => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Int(v) => out.push(*v),
                    other => return Err(mismatch(i, other, "Int")),
                }
            }
            builder
                .write_list_int(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::Float => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Float(v) => out.push(v.into_inner()),
                    // Int → Float promotion, matching the scalar arm.
                    Value::Int(v) => out.push(*v as f64),
                    other => return Err(mismatch(i, other, "Float")),
                }
            }
            builder
                .write_list_float(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::Bool => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Bool(v) => out.push(*v),
                    other => return Err(mismatch(i, other, "Bool")),
                }
            }
            builder
                .write_list_bool(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::String => {
            let mut out: Vec<&str> = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::String(s) => out.push(s.as_str()),
                    other => return Err(mismatch(i, other, "String")),
                }
            }
            builder
                .write_list_string(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::Schema { schema } => {
            write_list_schema_arg_into_builder(builder, name, schema, items)
        }
        TypeRepr::List { element: inner } => {
            relon_eval_api::buffer::write_nested_scalar_list(builder, name, inner, items)
                .map_err(buffer_to_runtime_error)
        }
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "cranelift-native: List element type {other:?} for arg `{name}` is not yet \
                 materialised (List<Int/Float/Bool/String/Schema> + List<List<scalar>>)"
            ),
        }),
    }
}

/// Marshal a `List<Schema>` arg: each element is a branded
/// `Value::Dict` written as a sub-record into the parent buffer's tail
/// through [`relon_eval_api::buffer::ListRecordWriter`]. The list
/// header's per-entry offsets and the inner sub-records' own pointer
/// slots are relocated into the parent's coordinate system by
/// `finish_entry` / `finish_list_record`. Mirrors the LLVM backend's
/// `marshal_list_schema_in`.
fn write_list_schema_arg_into_builder(
    builder: &mut BufferBuilder<'_>,
    name: &str,
    schema: &Schema,
    items: &[Value],
) -> Result<(), RuntimeError> {
    let elem_layout = SchemaLayout::offsets_for(schema).map_err(|e| RuntimeError::Unsupported {
        reason: format!("cranelift-native: List<Schema> arg `{name}` element layout: {e}"),
    })?;
    let mut writer = builder
        .list_record_writer(name, &elem_layout, schema)
        .map_err(buffer_to_runtime_error)?;
    for (i, it) in items.iter().enumerate() {
        let Value::Dict(dict) = it else {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native: List<Schema> arg `{name}` element #{i} got {} but expects \
                     a branded record",
                    it.type_name()
                ),
            });
        };
        let mut child = writer.start_entry();
        write_schema_fields_into_builder(&mut child, schema, dict, name)?;
        writer
            .finish_entry(builder, child)
            .map_err(buffer_to_runtime_error)?;
    }
    builder
        .finish_list_record(writer)
        .map_err(buffer_to_runtime_error)
}

/// Recursively fill a detached sub-record `child` with the fields of
/// `schema`, pulling each value out of the branded `dict`. Nested
/// `Schema`-typed fields recurse through [`write_value_into_builder`]'s
/// Schema arm, which re-enters this helper one layer down. The schema
/// name is threaded only for error messages. Mirrors the LLVM
/// backend's `write_schema_into_builder`.
fn write_schema_fields_into_builder(
    child: &mut BufferBuilder<'_>,
    schema: &Schema,
    dict: &relon_eval_api::ValueDict,
    parent_field: &str,
) -> Result<(), RuntimeError> {
    for sub_field in &schema.fields {
        let sub_value =
            dict.map
                .get(sub_field.name.as_str())
                .ok_or_else(|| RuntimeError::Unsupported {
                    reason: format!(
                        "cranelift-native: schema arg `{parent_field}` is missing field `{}`",
                        sub_field.name
                    ),
                })?;
        write_value_into_builder(child, sub_field, sub_value, &schema.name)?;
    }
    Ok(())
}

/// Decode a single field via [`BufferReader`]. Stage 3 widens this to
/// cover `String` / `List<Int>` / `List<Float>` / `List<Bool>` —
/// pointer-indirect leaves whose payload lives in the out_buf tail
/// area at the offset stored in the fixed-area slot. `List<String>` /
/// `List<Schema>` still surface as `Unsupported` because the codegen
/// can't yet emit them on the cranelift side.
fn read_value_from_reader(
    reader: &BufferReader<'_>,
    field: &Field,
    parent_schema: &Schema,
) -> Result<Value, RuntimeError> {
    match &field.ty {
        TypeRepr::Int => reader
            .read_int(&field.name)
            .map(Value::Int)
            .map_err(buffer_to_runtime_error),
        TypeRepr::Float => reader
            .read_float(&field.name)
            .map(|f| Value::Float(ordered_float_wrap(f)))
            .map_err(buffer_to_runtime_error),
        TypeRepr::Bool => reader
            .read_bool(&field.name)
            .map(Value::Bool)
            .map_err(buffer_to_runtime_error),
        TypeRepr::Null => reader
            .read_null(&field.name)
            .map(|()| Value::Null)
            .map_err(buffer_to_runtime_error),
        TypeRepr::String => reader
            .read_string(&field.name)
            .map(|s| Value::String(s.into()))
            .map_err(buffer_to_runtime_error),
        TypeRepr::List { element } => match element.as_ref() {
            TypeRepr::Int => reader
                .read_list_int(&field.name)
                .map(|v| Value::List(Arc::new(v.into_iter().map(Value::Int).collect())))
                .map_err(buffer_to_runtime_error),
            TypeRepr::Float => reader
                .read_list_float(&field.name)
                .map(|v| {
                    Value::List(Arc::new(
                        v.into_iter()
                            .map(|f| Value::Float(ordered_float_wrap(f)))
                            .collect(),
                    ))
                })
                .map_err(buffer_to_runtime_error),
            TypeRepr::Bool => reader
                .read_list_bool(&field.name)
                .map(|v| Value::List(Arc::new(v.into_iter().map(Value::Bool).collect())))
                .map_err(buffer_to_runtime_error),
            TypeRepr::String => reader
                .read_list_string(&field.name)
                .map(|v| {
                    Value::List(Arc::new(
                        v.into_iter().map(|s| Value::String(s.into())).collect(),
                    ))
                })
                .map_err(buffer_to_runtime_error),
            // `List<Schema>`: walk the pointer array into one sub-reader
            // per entry (`read_list_record` shares the same buffer base),
            // then drain each entry's fields into a branded dict via the
            // same `read_record_into_map` the top-level record return
            // uses. The recursion runs entirely in safe Rust against the
            // single shared buffer window — no machine-code re-pack.
            TypeRepr::Schema { schema } => {
                let elem_layout = SchemaLayout::offsets_for(schema).map_err(|e| {
                    RuntimeError::Unsupported {
                        reason: format!(
                            "cranelift-native: List<Schema> element `{}` layout: {e}",
                            schema.name
                        ),
                    }
                })?;
                let sub_readers = reader
                    .read_list_record(&field.name, &elem_layout, schema)
                    .map_err(buffer_to_runtime_error)?;
                let mut items = Vec::with_capacity(sub_readers.len());
                for sub in &sub_readers {
                    let map = read_record_into_map(sub, schema)?;
                    items.push(Value::branded_dict(map, Some(schema.name.clone())));
                }
                Ok(Value::List(Arc::new(items)))
            }
            // `List<List<scalar>>`: decode the nested pointer-array into
            // `Vec<Vec<Value>>` via the shared-base reader, then re-wrap
            // each inner row as a `Value::List`.
            TypeRepr::List { .. } => reader
                .read_list_list(&field.name)
                .map(|rows| {
                    Value::List(Arc::new(
                        rows.into_iter().map(|r| Value::List(Arc::new(r))).collect(),
                    ))
                })
                .map_err(buffer_to_runtime_error),
            other => Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native: cannot decode list field `{field}` of element type `{ty:?}` in schema `{schema}`",
                    field = field.name,
                    ty = other,
                    schema = parent_schema.name,
                ),
            }),
        },
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "cranelift-native: cannot decode field `{field}` of type `{ty:?}` in schema `{schema}`",
                field = field.name,
                ty = other,
                schema = parent_schema.name,
            ),
        }),
    }
}

/// Borrow `ordered_float::OrderedFloat` without the bound's crate
/// dependency leaking out of the helper — keeps the eval-api surface
/// of the cranelift backend narrow.
fn ordered_float_wrap(f: f64) -> ordered_float::OrderedFloat<f64> {
    ordered_float::OrderedFloat(f)
}

/// Drain every field of `schema` into a sorted `BTreeMap<SmolStr,
/// Value>`. Mirrors `relon_codegen_wasm::read_record_into_map`.
fn read_record_into_map(
    reader: &BufferReader<'_>,
    schema: &Schema,
) -> Result<BTreeMap<SmolStr, Value>, RuntimeError> {
    let mut map = BTreeMap::new();
    for field in &schema.fields {
        let value = read_value_from_reader(reader, field, schema)?;
        map.insert(SmolStr::from(field.name.as_str()), value);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Send + Sync sanity check so the AutoEvaluator path can hold a
    /// `Box<dyn Evaluator>` without surprises.
    #[test]
    fn cranelift_evaluator_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AotEvaluator>();
    }

    /// `sandbox_matches` is the cache-drift invalidation guard: its
    /// doc contract says it compares by *every* flag field. Pin that
    /// two configs differing only in `div_check` do NOT match.
    #[test]
    fn sandbox_matches_compares_every_flag() {
        let base = SandboxConfig::default();

        // Identical configs match.
        assert!(sandbox_matches(&base, &base.clone()));

        // Differing only in `div_check` must NOT match.
        let no_div = SandboxConfig {
            div_check: false,
            ..base.clone()
        };
        assert!(!sandbox_matches(&base, &no_div));
        assert!(!sandbox_matches(&no_div, &base));
    }

    /// 2026-05-21 dispatch-boundary lever (a) smoke: the SmallMap entry
    /// returns the same value the HashMap-keyed `run_main` does, and
    /// rejects mismatched arity / missing keys with typed errors.
    #[test]
    fn run_main_smallmap_matches_run_main() {
        use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
        use relon_parser::TokenRange;

        // Trivial #main(arg0: I64, arg1: I64) -> I64 returning arg0 + arg1.
        let body = vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Add(IrType::I64),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ];
        let ir = IrModule {
            imports: vec![],
            funcs: vec![Func {
                name: "run_main".to_string(),
                params: vec![IrType::I64, IrType::I64],
                ret: IrType::I64,
                body,
                range: TokenRange::default(),
            }],
            entry_func_index: Some(0),
            closure_table: vec![],
        };

        let eval = AotEvaluator::from_ir_direct(
            ir,
            SandboxConfig::default(),
            vec!["arg0".to_string(), "arg1".to_string()],
        )
        .expect("compile");

        // HashMap path (Evaluator trait).
        let mut hm = HashMap::with_capacity(2);
        hm.insert("arg0".to_string(), Value::Int(5));
        hm.insert("arg1".to_string(), Value::Int(7));
        let r_hm = eval.run_main(hm).expect("hashmap path");

        // SmallMap path.
        let r_sm = eval
            .run_main_smallmap(&[("arg0", Value::Int(5)), ("arg1", Value::Int(7))])
            .expect("smallmap path");

        assert_eq!(r_hm, r_sm);
        assert_eq!(r_sm, Value::Int(12));

        // Arity mismatch surfaces as Unsupported.
        let err = eval
            .run_main_smallmap(&[("arg0", Value::Int(1))])
            .expect_err("arity mismatch must error");
        assert!(matches!(err, RuntimeError::Unsupported { .. }));

        // Missing key surfaces as MissingMainArg.
        let err = eval
            .run_main_smallmap(&[("arg0", Value::Int(1)), ("bogus", Value::Int(2))])
            .expect_err("missing arg must error");
        assert!(matches!(err, RuntimeError::MissingMainArg { .. }));
    }
}
