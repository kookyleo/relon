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

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use cranelift_jit::JITModule;

use relon_eval_api::buffer::{BufferBuilder, BufferError, BufferReader};
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::{Node, TokenRange};

use crate::cache::CacheEntry;
use crate::codegen::{self, CompiledModule, EntryShape};
use crate::error::CraneliftError;
use crate::object_cache_integration as cache_int;
use crate::sandbox::{CapabilityVtable, SandboxConfig, SandboxState, TrapKind};

/// Type alias for the raw `extern "C"` entry the JIT produced when
/// the entry shape is [`EntryShape::LegacyI64Args`]. Five i64s cover
/// the v5-β-1 `#main(Int x, Int y, Int z, Int w)` envelope; longer
/// arities surface as `UnsupportedSignature` before the trampoline
/// tries to dispatch.
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
pub struct CraneliftAotEvaluator {
    /// Backing storage for the entry's machine code (JIT module or
    /// dlopen'd cache object). Kept alive for the evaluator's
    /// lifetime so the function pointers in `entry_fn` /
    /// `closure_table` stay valid.
    _module: EntryBacking,
    /// Raw function pointer to the JIT'd `run_main`. The exact shape
    /// is tracked in [`EntryPtr`].
    entry_fn: EntryPtr,
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
    /// Per-call sandbox state. Wrapped in `Arc` so concurrent
    /// `run_main` invocations from multiple threads can hand the JIT
    /// the same pointer without contention on the underlying
    /// allocation; the few atomic fields synchronise updates.
    sandbox_state: Arc<SandboxState>,
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
    /// `JITModule::get_finalized_function`. The sandbox state's
    /// `closure_table_base` is installed to point at this vec's
    /// element zero so `Op::CallClosure` can dereference the slot.
    ///
    /// Wrapped in `Box<[usize]>` so the address is stable for the
    /// evaluator's lifetime (we install a raw pointer into the
    /// sandbox state). Empty when the module has no lambdas.
    closure_table: Box<[usize]>,
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
    ///
    /// v5-β-2 widens this from a thin stub to the real end-to-end
    /// path. The lowering pass produces a buffer-protocol shaped IR
    /// (`#main` signature = `(I32, I32, I32, I32, I64) -> I32`) plus
    /// the canonical main / return schemas; both are captured on the
    /// evaluator so `run_main` can serialise / deserialise the
    /// buffers through the same `BufferBuilder` / `BufferReader`
    /// helpers the wasm-AOT backend uses.
    pub fn from_source(src: &str) -> Result<Self, CraneliftError> {
        let (ir_module, main_schema, return_schema) = Self::lower_source(src)?;
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
        let schema_entry = match crate::schema_cache::deserialize(&schema_bytes) {
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

    /// v5-γ stage 2: build a `CraneliftAotEvaluator` whose entry
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
        let sandbox_state = Arc::new(SandboxState::new(capabilities));
        sandbox_state.entry_range.set(entry_range);
        // SAFETY: closure_table is Box-allocated and lives on the
        // evaluator; the raw pointer stays valid for the evaluator's
        // lifetime.
        unsafe {
            let base = if closure_table.is_empty() {
                0
            } else {
                closure_table.as_ptr() as usize
            };
            sandbox_state.install_closure_table(base);
        }

        let arity = buffer_schema
            .as_ref()
            .map(|bs| bs.main_schema.fields.len())
            .unwrap_or(schema_entry.entry_arity as usize);

        Ok(Self {
            _module: EntryBacking::Dlopen(loaded),
            entry_fn,
            entry_arity: arity,
            param_names: schema_entry.param_names,
            entry_range,
            sandbox_state,
            buffer_schema,
            const_data: schema_entry.const_data,
            closure_table,
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

        // 3. Persist the schema cache so `from_cache_dir` can rebuild
        // the trampoline without re-parsing.
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
        let schema_bytes = match crate::schema_cache::serialize(&schema_entry) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "relon::object_cache",
                    "schema cache serialise failed: {e}; skipping schema cache write"
                );
                // Still try to persist object + ir for forward
                // compatibility; without schema cache, from_cache_dir
                // will fall back to from_source.
                if let Err(e) = cache_int::try_store_to_cache(
                    cache_dir,
                    source_hash,
                    &artifact.et_rel_bytes,
                    &metadata,
                    &ir_bytes,
                ) {
                    tracing::warn!(
                        target: "relon::object_cache",
                        "cache write returned unexpected error: {e}"
                    );
                }
                return;
            }
        };

        if let Err(e) = cache_int::try_store_to_cache(
            cache_dir,
            source_hash,
            &artifact.et_rel_bytes,
            &metadata,
            &ir_bytes,
        ) {
            tracing::warn!(
                target: "relon::object_cache",
                "cache write returned unexpected error: {e}"
            );
            return;
        }

        // Schema cache write is best-effort, but its absence forces a
        // fallback so we surface failures at `info`.
        let schema_path = crate::schema_cache::schema_cache_path_for(cache_dir, source_hash);
        if let Err(e) = std::fs::create_dir_all(cache_dir) {
            tracing::warn!(
                target: "relon::object_cache",
                "schema cache create_dir_all failed: {e}"
            );
            return;
        }
        let tmp = schema_path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        if let Err(e) = std::fs::write(&tmp, &schema_bytes) {
            tracing::warn!(
                target: "relon::object_cache",
                "schema cache tmp write failed: {e}"
            );
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &schema_path) {
            tracing::warn!(
                target: "relon::object_cache",
                "schema cache rename failed: {e}"
            );
            let _ = std::fs::remove_file(&tmp);
        } else {
            tracing::debug!(
                target: "relon::object_cache",
                "schema cache wrote {} bytes to {}",
                schema_bytes.len(),
                schema_path.display()
            );
        }
    }

    /// Fallback path: persist just the IR-cache half when the
    /// cranelift-object emit step fails. Same atomic-rename
    /// behaviour as the integration layer.
    fn write_ir_only(cache_dir: &Path, source_hash: [u8; 32], ir_bytes: &[u8]) {
        let path = cache_int::ir_cache_path_for(cache_dir, source_hash);
        if let Err(e) = std::fs::create_dir_all(cache_dir) {
            tracing::warn!(
                target: "relon::object_cache",
                "ir-cache-only create_dir_all failed: {e}"
            );
            return;
        }
        let tmp = path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        if let Err(e) = std::fs::write(&tmp, ir_bytes) {
            tracing::warn!(
                target: "relon::object_cache",
                "ir-cache-only tmp write failed: {e}"
            );
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            tracing::warn!(
                target: "relon::object_cache",
                "ir-cache-only rename failed: {e}"
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Internal helper: lower a source string into an IR module and
    /// surface the canonical main / return schemas the buffer
    /// protocol talks against. Mirrors
    /// `relon_codegen_wasm::WasmAotEvaluator::compile_source`.
    fn lower_source(src: &str) -> Result<(relon_ir::ir::Module, Schema, Schema), CraneliftError> {
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
        // Install the process-wide signal handler once at evaluator
        // construction. The handler is idempotent (`Once::call_once`
        // internally) so doing it here rather than per-invoke moves a
        // (cheap but non-zero) atomic-load probe off the dispatch hot
        // path. The `Once`'s state lives in the trap-handler module
        // and stays live for the host's lifetime.
        crate::trap_handler::install_global_signal_handler();

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

        // cap_bit width 64 mirrors the wasm-AOT side's
        // `relon_caps_avail` u64 bitmap shape. Hosts that register a
        // higher cap_bit cause `register` to grow the vector.
        let capabilities = Arc::new(CapabilityVtable::with_capacity(64));
        let sandbox_state = Arc::new(SandboxState::new(capabilities));
        sandbox_state.entry_range.set(entry_range);
        // SAFETY: closure_table is Box-allocated and lives on the
        // evaluator; the raw pointer stays valid for the evaluator's
        // lifetime. When the table is empty we install 0 (cranelift
        // never reads through it because no Op::CallClosure was
        // emitted).
        unsafe {
            let base = if closure_table.is_empty() {
                0
            } else {
                closure_table.as_ptr() as usize
            };
            sandbox_state.install_closure_table(base);
        }

        // Buffer-protocol arity equals the user-field count when we
        // have a schema; fall back to the IR-param count for legacy
        // / orphaned buffer modules.
        let arity = buffer_schema
            .as_ref()
            .map(|bs| bs.main_schema.fields.len())
            .unwrap_or(entry_arity);

        Ok(Self {
            _module: EntryBacking::Jit(module),
            entry_fn,
            entry_arity: arity,
            param_names,
            entry_range,
            sandbox_state,
            buffer_schema,
            const_data,
            closure_table,
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
        // Stage 5 Phase C.4: re-install the closure table pointer
        // onto the fresh state so `Op::CallClosure` keeps resolving.
        // SAFETY: the closure_table allocation lives on the
        // evaluator; its raw pointer remains valid for the
        // evaluator's lifetime.
        unsafe {
            let base = if self.closure_table.is_empty() {
                0
            } else {
                self.closure_table.as_ptr() as usize
            };
            new_state.install_closure_table(base);
        }
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
        if self.buffer_schema.is_some() {
            return Err(RuntimeError::Unsupported {
                reason: "cranelift-native: run_main_legacy_i64 requires a legacy-shape entry; the evaluator was built from buffer-protocol source"
                    .into(),
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
        if args.len() > 4 {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native legacy entry supports up to 4 args; got {}",
                    args.len()
                ),
            });
        }
        let mut argv = [0i64; 4];
        argv[..args.len()].copy_from_slice(args);
        self.invoke_legacy_entry(argv)
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
        if self.buffer_schema.is_some() {
            return Err(RuntimeError::Unsupported {
                reason: "cranelift-native: run_main_smallmap requires a legacy-shape entry; the evaluator was built from buffer-protocol source"
                    .into(),
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
        if args.len() > 4 {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native legacy entry supports up to 4 args; got {}",
                    args.len()
                ),
            });
        }
        let mut argv = [0i64; 4];
        for (i, name) in self.param_names.iter().enumerate() {
            // Linear scan: at most 4 entries, faster than a hash lookup
            // for tiny n. Accept either the declared param name or the
            // synthetic `arg{i}` form for hosts that haven't surfaced
            // names yet.
            let mut found: Option<&Value> = None;
            for (k, v) in args {
                if *k == name.as_str() {
                    found = Some(v);
                    break;
                }
            }
            if found.is_none() {
                // Fallback to `arg{i}` synthetic; format only on miss.
                let synth = format!("arg{i}");
                for (k, v) in args {
                    if *k == synth.as_str() {
                        found = Some(v);
                        break;
                    }
                }
            }
            let value = found.ok_or_else(|| RuntimeError::MissingMainArg {
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
        let result_i64 = self.invoke_legacy_entry(argv)?;
        Ok(Value::Int(result_i64))
    }

    /// Internal: invoke the legacy-shape JIT entry with the supplied
    /// i64 args.
    ///
    /// 2026-05-21 dispatch-boundary lever (b): the `catch_unwind`
    /// shield is now `cfg(debug_assertions)`-gated. Production /
    /// release builds call the JIT entry directly because:
    ///
    /// * cranelift codegen routes every guarded op through `cond_trap`
    ///   + a recorded `trap_code` — these become hardware traps
    ///   intercepted by the signal-hook handler, not Rust panics.
    /// * Helper-call symbols (`relon_now`, `relon_raise_trap`,
    ///   `relon_cap_lookup`) are audited to never panic on their hot
    ///   paths; they return error codes via the sandbox state instead.
    /// * The thread-local signal slot (`dispatch_post` reads it before
    ///   the trap_code) catches SIGSEGV / SIGFPE / SIGILL even without
    ///   `catch_unwind`.
    ///
    /// Debug builds keep `catch_unwind` so unit tests that exercise
    /// pathological codegen (e.g. helper-call panics regression suite)
    /// still surface a typed error rather than aborting the test
    /// process.
    fn invoke_legacy_entry(&self, args: [i64; 4]) -> Result<i64, RuntimeError> {
        // 2026-05-21 dispatch-boundary lever (c): lazy trap-code reset.
        // Debug builds keep the eager pre-invoke reset so a stale code
        // from a previous panicking test can't leak across runs; release
        // builds skip both resets because `dispatch_post_unshielded`
        // owns the cleanup (only stores back when the slot was found
        // non-zero, see `take_trap_code` and `take_signal_slot`).
        #[cfg(debug_assertions)]
        {
            crate::trap_handler::reset_thread_signal_slot();
            self.sandbox_state.reset_trap();
        }
        let state_ptr: *const SandboxState = Arc::as_ptr(&self.sandbox_state);
        let entry = match self.entry_fn {
            EntryPtr::Legacy(f) => f,
            EntryPtr::Buffer(_) => {
                return Err(RuntimeError::Unsupported {
                    reason: "cranelift-native: invoke_legacy_entry called on buffer-protocol shape"
                        .into(),
                });
            }
        };

        #[cfg(debug_assertions)]
        {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                entry(state_ptr, args[0], args[1], args[2], args[3])
            }));
            self.dispatch_post(result)
        }
        #[cfg(not(debug_assertions))]
        {
            // SAFETY: the signal-hook handler is process-wide-installed
            // at evaluator construction; SIGSEGV / SIGFPE / SIGILL from
            // the JIT body land in the thread-local slot which
            // `dispatch_post_unshielded` reads. Helper calls are audited
            // to never panic on their hot paths.
            let value = unsafe { entry(state_ptr, args[0], args[1], args[2], args[3]) };
            self.dispatch_post_unshielded(value)
        }
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
        // The process-wide signal handler is installed once at
        // evaluator construction (`from_ir_inner`); it stays live for
        // the host's lifetime, so the hot dispatch path doesn't pay a
        // `Once::call_once` atomic-load probe per invocation.
        //
        // 2026-05-21 dispatch-boundary lever (c): lazy trap-code reset.
        // See `invoke_legacy_entry` for the rationale; release builds
        // skip both resets and let `dispatch_post_unshielded` own the
        // cleanup.
        #[cfg(debug_assertions)]
        {
            crate::trap_handler::reset_thread_signal_slot();
            self.sandbox_state.reset_trap();
        }
        // SAFETY: `arena` is borrowed mutably here and stays valid
        // through the JIT call's lifetime (`arena` outlives this
        // function); the JIT side reads / writes through the pointer
        // we install. Both the host trampoline and the cranelift
        // emitter respect the `arena_len` field for bounds.
        unsafe {
            self.sandbox_state
                .install_arena(arena.as_mut_ptr(), arena.len() as u32);
            self.sandbox_state.install_scratch_base(scratch_base);
        }
        let state_ptr: *const SandboxState = Arc::as_ptr(&self.sandbox_state);
        let entry = match self.entry_fn {
            EntryPtr::Buffer(f) => f,
            EntryPtr::Legacy(_) => {
                return Err(RuntimeError::Unsupported {
                    reason: "cranelift-native: invoke_buffer_entry called on legacy shape".into(),
                });
            }
        };

        // 2026-05-21 dispatch-boundary lever (b): cfg-gate the
        // catch_unwind shield to debug builds. See `invoke_legacy_entry`
        // for the audit; the buffer-protocol entry shares the same
        // helper-call surface so the same reasoning applies.
        #[cfg(debug_assertions)]
        {
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
            self.dispatch_post(result)
        }
        #[cfg(not(debug_assertions))]
        {
            let value = unsafe {
                entry(
                    state_ptr,
                    in_ptr as i32,
                    in_len as i32,
                    out_ptr as i32,
                    out_cap as i32,
                    caps as i64,
                )
            };
            self.dispatch_post_unshielded(value)
        }
    }

    /// 2026-05-21 dispatch-boundary lever (b) helper: release-build
    /// post-call processing. Same checks as [`Self::dispatch_post`]
    /// minus the panic unwrap step. The JIT body was called outside a
    /// `catch_unwind` so `value` is already the raw return; we still
    /// consult the thread-local signal slot and the sandbox-state
    /// `trap_code` so hardware traps and JIT-side `cond_trap`s surface
    /// as typed errors.
    ///
    /// 2026-05-21 lever (c): owns the lazy-reset side of the lazy
    /// trap-code protocol. Uses `take_trap_code` (load-then-store-iff-
    /// nonzero) and only resets the thread-local signal slot when a
    /// signal actually fired, so the success path is two predictable-
    /// not-taken branches with no atomic stores.
    #[cfg(not(debug_assertions))]
    fn dispatch_post_unshielded<T>(&self, value: T) -> Result<T, RuntimeError> {
        let signal_code = crate::trap_handler::read_thread_signal_slot();
        if signal_code != 0 {
            // Reset the slot eagerly so the next dispatch doesn't pick
            // up a stale signal code.
            crate::trap_handler::reset_thread_signal_slot();
            if let Some(kind) = crate::trap_handler::signal_to_trap_kind(signal_code) {
                return Err(kind.to_runtime_error(self.entry_range));
            }
        }
        let code = self.sandbox_state.take_trap_code();
        if code != 0 {
            return Err(TrapKind::from_code(code as u8).to_runtime_error(self.entry_range));
        }
        Ok(value)
    }

    /// Post-process a JIT-call result: surface typed traps recorded in
    /// `state.trap_code`, otherwise pass the raw return value through.
    ///
    /// Stage 5 Phase C.3: also consults the thread-local signal slot
    /// populated by `crate::trap_handler` when a SIGSEGV / SIGFPE /
    /// SIGILL fired during the JIT call. Signal-side traps take
    /// precedence over the JIT-recorded trap code because the
    /// signal observation came from the hardware / OS layer which
    /// our codegen guards can't intercept.
    ///
    /// 2026-05-21 lever (b): only used by the debug-build dispatch
    /// path; release builds skip the `catch_unwind` and call
    /// `dispatch_post_unshielded` instead.
    #[cfg(debug_assertions)]
    fn dispatch_post<T>(&self, result: std::thread::Result<T>) -> Result<T, RuntimeError> {
        // Check the signal-hook slot first — a SIGSEGV during JIT
        // body execution should surface as a typed trap even if the
        // JIT-side `cond_trap` sequence never ran.
        let signal_code = crate::trap_handler::read_thread_signal_slot();
        if signal_code != 0 {
            if let Some(kind) = crate::trap_handler::signal_to_trap_kind(signal_code) {
                return Err(kind.to_runtime_error(self.entry_range));
            }
        }
        match result {
            Ok(v) => {
                let code = self.sandbox_state.trap_code();
                if code != 0 {
                    return Err(TrapKind::from_code(code as u8).to_runtime_error(self.entry_range));
                }
                Ok(v)
            }
            Err(payload) => {
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
        let out_cap = align_up(out_cap_min.max(64) + out_cap_cushion, 8);

        let const_data_len = u32::try_from(self.const_data.len()).map_err(|_| {
            RuntimeError::IoError("cranelift const-data section exceeds u32 range".into())
        })?;
        let in_ptr = align_up(const_data_len, 8);
        let out_ptr = align_up(in_ptr + in_len, 8);
        // Reserve a scratch region past the output buffer for the
        // memory stdlib (`concat` / `substring` / …) to bump-allocate
        // temporary records. 64 KiB matches the wasm side's typical
        // growth before it has to grow `memory`. The size is fixed
        // for now; later v5-γ work can pool / size it from the source
        // when stdlib usage gets bigger.
        let scratch_size: u32 = 65_536;
        let scratch_base = align_up(out_ptr + out_cap, 8);
        let arena_size = scratch_base
            .checked_add(scratch_size)
            .ok_or_else(|| RuntimeError::IoError("cranelift arena size overflow".into()))?;

        let mut arena = vec![0u8; arena_size as usize];
        if !self.const_data.is_empty() {
            arena[..self.const_data.len()].copy_from_slice(&self.const_data);
        }
        arena[in_ptr as usize..in_ptr as usize + in_bytes.len()].copy_from_slice(&in_bytes);

        // 3. Invoke the JIT.
        let bytes_written = self.invoke_buffer_entry_with_scratch(
            &mut arena,
            in_ptr,
            in_len,
            out_ptr,
            out_cap,
            scratch_base,
            /*caps=*/ 0,
        )?;
        if bytes_written < 0 {
            return Err(RuntimeError::IoError(format!(
                "cranelift run_main reported negative bytes_written: {bytes_written}"
            )));
        }
        let bw = bytes_written as usize;

        // 4. Decode the output region back to a `Value`. We always
        // give the reader at least `return_root_size` bytes — the
        // wasm side does the same.
        let read_len = bw.max(bs.return_layout.root_size);
        let read_end = out_ptr as usize + read_len;
        if read_end > arena.len() {
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
        // Buffer-protocol path: schema-driven serialisation through
        // `BufferBuilder` / `BufferReader`. Selected when the source
        // came in via `from_source`.
        if self.buffer_schema.is_some() {
            return self.run_main_buffer(args);
        }

        // Legacy direct-IR path: pack i64 args into the JIT's
        // `extern "C"` slot.
        if args.len() > 4 {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "cranelift-native legacy entry supports up to 4 args; got {}",
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

        let mut argv = [0i64; 4];
        for (i, name) in self.param_names.iter().enumerate() {
            let value = args.get(name).or_else(|| args.get(&format!("arg{i}")));
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

/// Round `value` up to the next multiple of `align`. `align` is
/// expected to be a power of two.
fn align_up(value: u32, align: u32) -> u32 {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
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
            .map(|s| Value::String(s.to_string()))
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

/// Drain every field of `schema` into a sorted `BTreeMap<String,
/// Value>`. Mirrors `relon_codegen_wasm::read_record_into_map`.
fn read_record_into_map(
    reader: &BufferReader<'_>,
    schema: &Schema,
) -> Result<BTreeMap<String, Value>, RuntimeError> {
    let mut map = BTreeMap::new();
    for field in &schema.fields {
        let value = read_value_from_reader(reader, field, schema)?;
        map.insert(field.name.clone(), value);
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
        assert_send_sync::<CraneliftAotEvaluator>();
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

        let eval = CraneliftAotEvaluator::from_ir_direct(
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
