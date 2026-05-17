//! Phase 8: wasm-AOT backend implementation of [`relon_eval_api::Evaluator`].
//!
//! [`WasmAotEvaluator`] drives a precompiled wasm module through the
//! binary handshake the codegen pass laid down (`run_main(in_ptr,
//! in_len, out_ptr, out_cap) -> bytes_written`). The struct keeps both
//! the parsed [`WasmModule`] (so it can translate traps and read
//! `relon.abi` metadata) and a `wasmtime::Module` (so it can
//! instantiate cheap per-call sessions without re-decoding wasm bytes).
//!
//! Scope (locked at Phase 8):
//!
//! * Only `run_main` is real. The other four [`Evaluator`] methods
//!   (`eval` / `eval_root` / `force_thunk` / `invoke_closure`) return
//!   [`RuntimeError::Unsupported`] — the wasm AOT pipeline consumes
//!   the AST at compile time, leaves nothing to evaluate at runtime,
//!   and the static topo-sort means there are no live thunks or
//!   closures to drive.
//! * Single-file source only. `#import`-spanning workspaces are a
//!   Phase 9 goal; the construction path runs the per-file analyzer.
//! * Schema field types supported: `Int`, `Float`, `Bool`, `Null`,
//!   `String`, `List<Int>`, plus nested branded `Schema { ... }` for
//!   the dict-return path. Anything else surfaces as
//!   [`BuildError::UnsupportedFieldType`] up front (matching the IR
//!   lowering's own supported leaves).

use crate::cache::{AotCache, CacheError, CachedSchemas};
use crate::{compile_lowered_entry, WasmModule};
use relon_eval_api::buffer::{BufferBuilder, BufferError, BufferReader};
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::{Capabilities, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::Node;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use thiserror::Error;
use wasmtime::{
    Config, Engine, InstancePre, Linker, Memory, Module as WtModule, Store, TypedFunc, Val,
};

/// Build the default [`wasmtime::Config`] used by every engine this
/// crate constructs internally. The single switch the host cares about
/// at this layer is fuel consumption: v3+ a-1 wires
/// [`WasmAotEvaluator::with_fuel_limit`] so a host can cap how much
/// wasm work a single `run_main` is allowed to perform. Fuel is opt-in
/// per evaluator (default budget `0` = unlimited, see
/// [`WasmAotEvaluator::fuel_limit`]) but the engine has to be primed
/// with `consume_fuel(true)` at construction time — wasmtime injects
/// the per-instruction fuel-decrement bookkeeping at compile time, so
/// flipping the bit later would be a no-op for already-compiled
/// modules.
///
/// Keeping the bookkeeping on unconditionally costs a single extra
/// load+subtract per wasm instruction. The Phase a-1 bench shows the
/// hot-path overhead is well under one percent for the warm-invoke
/// scenarios; preserving a single engine-shape across "limit / no
/// limit" callers is the simpler contract.
fn make_fuel_aware_config() -> Config {
    let mut cfg = Config::default();
    cfg.consume_fuel(true);
    cfg
}

/// Build a fresh [`wasmtime::Engine`] with fuel consumption enabled.
/// Used by [`AotCache::open`] and [`shared_default_engine`] so every
/// evaluator the host constructs through the public surface has the
/// same fuel-aware engine shape.
pub(crate) fn make_fuel_aware_engine() -> Engine {
    Engine::new(&make_fuel_aware_config()).expect(
        "wasmtime engine with fuel consumption enabled must construct; \
         default Config is always valid",
    )
}

/// Process-wide [`wasmtime::Engine`] used by [`WasmAotEvaluator::
/// from_source`] and [`WasmAotEvaluator::from_bytes`] — i.e. the
/// non-cache paths. Phase 9.c-2: avoids re-paying the ~50-100 μs
/// `Engine::default()` setup on every evaluator construction.
///
/// `wasmtime::Engine` clones are cheap `Arc` bumps and the engine is
/// `Send + Sync`, so sharing one across threads is safe. Hosts that
/// need a custom `wasmtime::Config` should drive the cache path
/// (`AotCache::open_with_engine`) instead — touching the global engine
/// would surprise unrelated callers that also use the default path.
///
/// Phase a-1: built with [`make_fuel_aware_engine`] so the
/// `with_fuel_limit` switch can take effect without rebuilding the
/// engine (wasmtime would otherwise refuse `set_fuel` on a non-fuel
/// engine).
fn shared_default_engine() -> &'static Engine {
    static SHARED: OnceLock<Engine> = OnceLock::new();
    SHARED.get_or_init(make_fuel_aware_engine)
}

/// Default `out_cap` (in bytes) when a host doesn't override it.
/// 4 KiB matches the codegen's `DATA_SECTION_BASE` and is plenty for
/// the v1 single-record return shapes (no recursive aggregates yet).
/// Hosts that emit larger tail records can supply a fresh evaluator
/// configured via [`WasmAotEvaluator::with_out_cap`].
const DEFAULT_OUT_CAP: u32 = 4096;

/// Errors surfaced while building a [`WasmAotEvaluator`].
///
/// The construction pipeline runs parse → per-file analyzer → IR
/// lowering → wasm codegen → wasmtime compile. Each stage's failure
/// shape lands in a dedicated variant so callers can route diagnostics
/// without losing the staging information.
#[derive(Debug, Error)]
pub enum BuildError {
    /// `parse_document` rejected the source. Carries the parser
    /// error's display form (the parser surface itself isn't
    /// re-exported here because hosts already touch it through
    /// `relon_parser::ParseDocumentError`).
    #[error("parse error: {0}")]
    ParseError(String),
    /// The per-file analyzer reported one or more `Error`-severity
    /// diagnostics. Joined into a single message so the BuildError
    /// stays a single thiserror enum surface.
    #[error("analyzer reported errors:\n  - {0}")]
    AnalyzerError(String),
    /// IR lowering failed (missing `#main`, unsupported type in a
    /// schema field, ...).
    #[error("lowering error: {0}")]
    LoweringError(String),
    /// Wasm codegen failed — usually a hand-built IR that escaped
    /// the lowering pass's invariants.
    #[error("codegen error: {0}")]
    CodegenError(String),
    /// [`WasmModule::from_bytes`] failed to decode the emitted
    /// custom sections. Indicates a codegen bug or a corrupted
    /// custom section.
    #[error("wasm load error: {0}")]
    WasmLoadError(String),
    /// `wasmtime::Module::new` or `Engine::default` failed to
    /// JIT-compile / validate the emitted module. Stringified so
    /// the public surface doesn't propagate `wasmtime::Error`.
    #[error("wasm instantiate error: {0}")]
    WasmInstantiateError(String),
    /// The supplied schema field type is not yet wired through the
    /// `BufferBuilder` / `BufferReader` path. The supported leaves
    /// match the IR lowering's allowed set: Int / Float / Bool /
    /// Null / String / List<Int> / nested Schema.
    #[error("unsupported field type `{type_label}` in field `{field}` (wasm-aot v1)")]
    UnsupportedFieldType {
        /// Field name that carries the unsupported type.
        field: String,
        /// Human-readable type label (`"Option"`, `"Result"`, ...).
        type_label: &'static str,
    },
    /// On-disk AOT cache I/O / serialisation failure. Distinguished
    /// from [`BuildError::WasmLoadError`] because the cache failure
    /// is recoverable — callers can drop the cache and retry from a
    /// clean compile if the host wants to.
    #[error("cache error: {0}")]
    CacheError(String),
}

/// A fully prepared wasmtime session — store, instance, exported
/// memory + `run_main` function handle, and the byte offsets the host
/// uses for `in_buf` / `out_buf` placement. Sessions are pooled across
/// `run_main` calls so the per-call cost stays at the actual buffer
/// marshalling + wasm execution, not at `Store::new` +
/// `InstancePre::instantiate`.
///
/// Session reuse is safe because:
///
/// * The module's exported `memory` belongs to the session's `Store`;
///   we overwrite the `in_buf` and `out_buf` regions on every call so
///   stale bytes from a previous invocation are harmless.
/// * The const-data section (string literals, list literals) sits
///   below `in_ptr` and is never mutated by the wasm body — see the
///   `relon_data_top` export the codegen pins.
/// * The capability bitmap is now passed through the run_main `i64`
///   argument (Phase 11) instead of an imported global, so the
///   session does not snapshot the host's grant set at construction
///   time and the same session safely serves calls under different
///   bitmaps when `Self::with_capabilities` swaps the evaluator's
///   `cap_grants` mid-life.
///
/// Phase 9.b-1: pre-grows the memory at session creation so the hot
/// path never needs `memory.grow`, eliminating the size-check branch
/// from every `run_main`.
struct WasmSession {
    /// Wasmtime store the instance lives in. Held by value so the
    /// Session owns its lifetime; sessions are pooled, not shared.
    store: Store<()>,
    /// Exported linear memory handle. Re-resolved once at construction
    /// and stashed so we don't pay an export lookup per call.
    memory: Memory,
    /// Typed `run_main` entry point — saves a `get_typed_func` call
    /// per invocation. Phase 11 signature: the trailing i64 is the
    /// per-call capability bitmap the wasm side forwards into its
    /// internal `relon_caps_avail` global.
    run_main: TypedFunc<(i32, i32, i32, i32, i64), i32>,
    /// Pre-computed input buffer base (aligned past the const-data
    /// section).
    in_ptr: u32,
    /// Pre-computed output buffer base (aligned past `in_ptr +
    /// in_cap`). For Phase 9.b-1 we size the input region to whatever
    /// the largest call so far needed; `out_ptr` is recomputed on the
    /// first call if a larger input slots forces it forward. Initially
    /// we anchor it at `data_top + a small fixed in-cap`.
    out_ptr: u32,
    /// In-buffer capacity reserved between `in_ptr` and `out_ptr`. The
    /// pool grows this when a call hands over more input bytes than
    /// the current reservation, which forces an `out_ptr` realignment.
    in_cap: u32,
}

/// Phase 8 wasm-AOT backend, Phase 9.b-1 pool-of-stores refactor,
/// Phase 11 cross-store `InstancePre` reuse.
///
/// Holds a precompiled wasm module plus the schemas / layouts the host
/// needs to bridge `Value` ↔ binary handshake. Implements
/// [`Evaluator`] with `run_main` as the only real entry point.
///
/// Per-call cost was historically dominated by `Store::new` +
/// `Linker::instantiate` (≈ 40 μs on the bench laptop). Phase 9.b-1
/// folded those into a pooled [`WasmSession`] that the evaluator
/// recycles across invocations. Phase 11 then lifted the
/// `Linker`-built [`wasmtime::InstancePre`] onto the evaluator itself
/// so a fresh session only pays for `Store::new` + a single
/// `InstancePre::instantiate`; the linker's import-resolution work is
/// amortised across every store the evaluator ever opens.
///
/// The session is a `Mutex<Vec<...>>` free-list:
///
/// * `run_main` pops a session if one is available, otherwise builds a
///   fresh one through [`Self::build_session`].
/// * On return the session is pushed back, so the next call (same
///   thread or another) gets a warm one for free.
/// * Parallel callers each grab their own session — the pool grows as
///   contention demands and stays small for single-threaded workloads.
pub struct WasmAotEvaluator {
    /// Parsed wasm module wrapping the raw bytes + decoded
    /// `relon.abi` / `relon.srcmap` / `relon.host_fns` / `relon.uctab`
    /// sections. Used for trap translation.
    module: WasmModule,
    /// Wasmtime compilation engine. Kept on the evaluator so per-call
    /// `instantiate` doesn't pay for engine setup.
    engine: Engine,
    /// Phase 11: pre-validated linker → module binding, ready to
    /// stamp into any [`Store`] the evaluator opens. The wasm module
    /// no longer imports `relon_caps_avail`, so the linker's
    /// import-resolution state is now fully store-independent and a
    /// single `InstancePre` serves every pooled session.
    instance_pre: InstancePre<()>,
    /// Canonical `#main` param schema. Used for `BufferBuilder`
    /// construction.
    main_schema: Schema,
    /// Canonical return schema. Used for `BufferReader` construction.
    return_schema: Schema,
    /// Precomputed layout for `main_schema`.
    main_layout: OffsetTable,
    /// Precomputed layout for `return_schema`.
    return_layout: OffsetTable,
    /// Out buffer capacity in bytes used for each `run_main` call.
    /// Host can override via [`Self::with_out_cap`] before evaluating.
    out_cap: u32,
    /// Capability grant bitmap forwarded into the wasm module's
    /// internal `relon_caps_avail` global through the `run_main` i64
    /// argument. Phase 11 demoted the bitmap from an imported global
    /// to a per-call argument; this field is read fresh on every
    /// `run_main` call (no per-session snapshot) so
    /// [`Self::with_capabilities`] takes effect without resetting the
    /// session pool. Bit positions follow
    /// [`relon_eval_api::CapabilityBit`]. Defaults to the zero-trust
    /// grant set (`Capabilities::default`).
    cap_grants: u64,
    /// Phase a-1: per-`run_main` wasm step budget enforced via
    /// wasmtime's `Store::set_fuel`. `0` means unlimited — the hot
    /// path skips the `set_fuel` call entirely so non-budgeted hosts
    /// keep their previous overhead. A non-zero value re-stamps the
    /// session's store with that fuel count before every dispatch, so
    /// each `run_main` gets a fresh budget independent of prior calls
    /// (wasmtime decrements fuel monotonically — without a reset the
    /// pool's second call would inherit whatever the first call left
    /// behind, which is almost never what a host wants).
    ///
    /// Bit positions: 1 fuel unit ≈ 1 wasm instruction; `nop` / `drop`
    /// / `block` / `loop` are free. See wasmtime's `Store::set_fuel`
    /// docs for the full table. The unit is **not** wall-clock time
    /// nor cycles — choose a limit that matches the entry's wasm
    /// instruction count, not its expected latency.
    fuel_limit: u64,
    /// Free-list of warmed-up [`WasmSession`]s. Per-call cost on a hit
    /// is just the buffer marshal + wasm dispatch; misses pay the
    /// `Store::new` + `InstancePre::instantiate` price once.
    session_pool: Mutex<Vec<WasmSession>>,
}

impl WasmAotEvaluator {
    /// Compile `src` through a disk-backed [`AotCache`]. The fast path
    /// avoids both the codegen pipeline (parse / analyze / lower /
    /// codegen) *and* cranelift JIT when the cache holds a matching
    /// `.native` sidecar:
    ///
    ///  * three-way hit (`.wasm` + `.schemas` + matching `.native`)
    ///    → `wasmtime::Module::deserialize`, no JIT.
    ///  * partial hit (`.wasm` + `.schemas`, no native or drifted
    ///    native) → `wasmtime::Module::new` (JIT) + write a fresh
    ///    `.native` sidecar for the next call.
    ///  * full miss → run the codegen pipeline, write all sidecars,
    ///    return the evaluator.
    ///
    /// The cache key is `sha256(src)`. Cache validity is gated by the
    /// persisted `abi_version` + `codegen_version` for the wasm side
    /// and by the host's wasmtime version + target triple for the
    /// native side — see [`crate::cache`] for the full invalidation
    /// matrix.
    ///
    /// Returns the same [`WasmAotEvaluator`] shape as
    /// [`Self::from_source`] so callers can drop the cache in front of
    /// an existing call site without touching the post-construction
    /// surface.
    pub fn from_source_with_cache(src: &str, cache: &AotCache) -> Result<Self, BuildError> {
        let source_hash = AotCache::source_hash(src);
        // Cache hit short-circuits the entire compile pipeline. The
        // cached entry carries the canonical `(main, return)` schemas
        // it was originally compiled against, so the rehydration path
        // pulls them straight off disk instead of re-running parse /
        // analyze / lowering.
        if let Some(entry) = cache.load(source_hash).map_err(cache_err)? {
            if let Some(schemas) = entry.schemas {
                // The `.native` sidecar is independent of `.schemas` —
                // a hit there means cranelift already JIT'd this exact
                // wasm under the current wasmtime version + target
                // triple, so we can hand the bytes straight to
                // `Module::deserialize` and skip the JIT entirely.
                let native = cache.load_native(source_hash).map_err(cache_err)?;
                // Phase 9.c-2: borrow the cache-owned engine instead
                // of paying `Engine::default()` again. Engine clone is
                // a cheap Arc bump; the actual cranelift / target ISA
                // state stays shared. This is the change that pushes
                // cached cold start under 100 μs.
                let engine = cache.engine().clone();
                // Three-way decision tree:
                //   * native blob present + deserialise OK  → no JIT, no write-back.
                //   * native blob present + deserialise Err → silently
                //     fall back to JIT (the on-disk blob is corrupted
                //     or somehow slipped past the compat hash check);
                //     write a fresh blob back so the next call hits
                //     the fast path.
                //   * no native blob                        → JIT +
                //     write the blob for the next call.
                // The corruption fallback is what makes the cache
                // self-healing: hosts that hit transient FS issues
                // (partial write, NFS truncation) recover on the
                // following run rather than panicking forever.
                let (compiled, write_back_native) = match native {
                    Some(blob) => match deserialize_native(&engine, &blob.bytes) {
                        Ok(m) => (m, false),
                        Err(_) => (
                            WtModule::new(&engine, &entry.wasm_bytes)
                                .map_err(|e| BuildError::WasmInstantiateError(e.to_string()))?,
                            true,
                        ),
                    },
                    None => (
                        WtModule::new(&engine, &entry.wasm_bytes)
                            .map_err(|e| BuildError::WasmInstantiateError(e.to_string()))?,
                        true,
                    ),
                };
                // Best-effort: write the freshly JIT'd native blob
                // back so the next cold start hits the deserialize
                // fast path. A serialise / write failure is logged
                // via the error path rather than fatal because the
                // evaluator is already fully constructed at this
                // point — falling over on a cache write would surprise
                // hosts that happen to evaluate from a read-only
                // mount.
                if write_back_native {
                    if let Ok(native_bytes) = compiled.serialize() {
                        let _ = cache.store_native(source_hash, &native_bytes);
                    }
                }
                return Self::from_parts(
                    engine,
                    compiled,
                    entry.wasm_bytes,
                    schemas.main,
                    schemas.return_,
                );
            }
            // The cached `.meta` was written by a schemaless `store`
            // call. We have wasm bytes but no schemas — fall through
            // to the full pipeline so we still produce a valid
            // evaluator. The fresh `store_with_schemas` writes the
            // sidecar so the next call hits the fast path.
        }
        let (lowered, bytes) = Self::compile_source(src)?;
        let main_schema = lowered.main_schema;
        let return_schema = lowered.return_schema;
        let schema_hash = combined_schema_hash(&main_schema, &return_schema);
        // Persist the freshly compiled module + schemas before handing
        // the evaluator back. A failure here is a hard error: the host
        // explicitly asked for a cache, and silently dropping the write
        // would hide a misconfigured cache directory.
        cache
            .store_with_schemas(
                source_hash,
                &bytes,
                schema_hash,
                &CachedSchemas {
                    main: main_schema.clone(),
                    return_: return_schema.clone(),
                },
            )
            .map_err(cache_err)?;
        // Build the evaluator off the cache's shared engine so the
        // miss path matches the hit path's engine identity. Engine
        // clone is an Arc bump — cheap, no cranelift re-setup.
        let evaluator = Self::from_bytes_with_engine(
            cache.engine().clone(),
            bytes,
            main_schema,
            return_schema,
        )?;
        // Best-effort: persist the cranelift-emitted native code so the
        // *next* cold start of this same source skips the JIT entirely
        // through `Module::deserialize`. Write failures are silent
        // (read-only mount, transient ENOSPC, …); they leave the
        // existing `.wasm` / `.schemas` sidecars in place, so a
        // subsequent run still hits the partial-hit JIT-then-save path
        // and gets another chance to produce the sidecar.
        if let Ok(native_bytes) = evaluator.instance_pre.module().serialize() {
            let _ = cache.store_native(source_hash, &native_bytes);
        }
        Ok(evaluator)
    }

    /// Build an evaluator from an already-prepared engine / compiled
    /// module pair plus the canonical schemas. Used by the cache-hit
    /// fast path so it can hand in a `Module::deserialize`-rehydrated
    /// module without paying for JIT compilation a second time. Wasm
    /// bytes are still required for `WasmModule::from_bytes` (which
    /// parses the `relon.abi` / `relon.srcmap` / `relon.uctab` custom
    /// sections we need at runtime).
    fn from_parts(
        engine: Engine,
        compiled: WtModule,
        wasm_bytes: Vec<u8>,
        main_schema: Schema,
        return_schema: Schema,
    ) -> Result<Self, BuildError> {
        Self::reject_unsupported_fields(&main_schema)?;
        Self::reject_unsupported_fields(&return_schema)?;
        let main_layout = SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| BuildError::LoweringError(format!("main schema layout: {e}")))?;
        let return_layout = SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| BuildError::LoweringError(format!("return schema layout: {e}")))?;
        let module = WasmModule::from_bytes(wasm_bytes)
            .map_err(|e| BuildError::WasmLoadError(e.to_string()))?;
        let instance_pre = build_instance_pre(&engine, &compiled)?;
        Ok(Self {
            module,
            engine,
            instance_pre,
            main_schema,
            return_schema,
            main_layout,
            return_layout,
            out_cap: DEFAULT_OUT_CAP,
            cap_grants: Capabilities::default().to_cap_bitmap(),
            fuel_limit: 0,
            session_pool: Mutex::new(Vec::new()),
        })
    }

    /// Compile `src` end-to-end and return a ready-to-call evaluator.
    ///
    /// Pipeline: `parse_document` → `relon_analyzer::analyze` →
    /// `relon_ir::lower_workspace_single` → `compile_lowered_entry` →
    /// `WasmModule::from_bytes` → `wasmtime::Module::new`.
    ///
    /// Single-file path: any `#import` in `src` is rejected because the
    /// pipeline never opens its target. Use [`Self::from_workspace`]
    /// when the entry pulls in transitive modules.
    pub fn from_source(src: &str) -> Result<Self, BuildError> {
        let (lowered, bytes) = Self::compile_source(src)?;
        Self::from_bytes(bytes, lowered.main_schema, lowered.return_schema)
    }

    /// Phase 10-b: compile a workspace whose entry file pulls in
    /// transitive `#import "./..."` modules.
    ///
    /// The caller (typically the CLI / facade) is expected to have
    /// driven the analyzer through `relon_analyzer::analyze_entry` so
    /// every reachable module is parsed + analyzed and the workspace's
    /// `has_errors()` gate is clean. This entry then:
    ///
    /// 1. Looks up `entry_module` in `ws.modules` / `ws.nodes`.
    /// 2. Calls `relon_ir::lower_workspace` which now merges every
    ///    reachable module's `#schema` declarations into one cross-file
    ///    resolver so `#main(User u)` resolves `User` from
    ///    `./util.relon`.
    /// 3. Hands the resulting IR + canonical schemas off to the same
    ///    codegen + wasmtime pipeline `from_source` uses.
    ///
    /// Errors surface as the same `BuildError` shape `from_source`
    /// returns; `LoweringError::DuplicateSchemaAcrossFiles` /
    /// `MultipleMainDirectives` lift into `BuildError::LoweringError`
    /// so callers route them through one display path.
    pub fn from_workspace(
        ws: &relon_analyzer::workspace::WorkspaceTree,
        entry_module: &str,
    ) -> Result<Self, BuildError> {
        if !ws.modules.contains_key(entry_module) {
            return Err(BuildError::LoweringError(format!(
                "entry module `{entry_module}` not found in workspace"
            )));
        }
        let lowered = relon_ir::lower_workspace(ws, entry_module)
            .map_err(|e| BuildError::LoweringError(e.to_string()))?;
        let bytes =
            compile_lowered_entry(&lowered).map_err(|e| BuildError::CodegenError(e.to_string()))?;
        Self::from_bytes(bytes, lowered.main_schema, lowered.return_schema)
    }

    /// Run parse / analyze / lower / codegen on `src` and return the
    /// `LoweredEntry` (for schemas) alongside the emitted wasm bytes.
    /// Shared between [`Self::from_source`] and
    /// [`Self::from_source_with_cache`] so both paths report identical
    /// errors and stay in lockstep when the pipeline changes.
    fn compile_source(src: &str) -> Result<(relon_ir::LoweredEntry, Vec<u8>), BuildError> {
        let ast =
            relon_parser::parse_document(src).map_err(|e| BuildError::ParseError(e.to_string()))?;
        let analyzed = relon_analyzer::analyze(&ast);
        if analyzed.has_errors() {
            let joined = analyzed
                .diagnostics
                .iter()
                .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("\n  - ");
            return Err(BuildError::AnalyzerError(joined));
        }
        let lowered = relon_ir::lower_workspace_single(&analyzed, &ast)
            .map_err(|e| BuildError::LoweringError(e.to_string()))?;
        let bytes =
            compile_lowered_entry(&lowered).map_err(|e| BuildError::CodegenError(e.to_string()))?;
        Ok((lowered, bytes))
    }

    /// Build an evaluator from already-emitted wasm bytes plus the
    /// canonical schemas they were compiled against. Useful when a
    /// host caches the compiled output and re-loads it from disk.
    ///
    /// Phase 9.c-2: borrows the process-wide shared
    /// [`wasmtime::Engine`] returned by `shared_default_engine` so
    /// repeated `from_bytes` (or `from_source`) calls don't pay the
    /// `Engine::default()` setup cost more than once. Hosts that need
    /// a custom engine should go through
    /// [`Self::from_bytes_with_engine`] / the [`AotCache`] surface.
    pub fn from_bytes(
        bytes: Vec<u8>,
        main_schema: Schema,
        return_schema: Schema,
    ) -> Result<Self, BuildError> {
        Self::from_bytes_with_engine(
            shared_default_engine().clone(),
            bytes,
            main_schema,
            return_schema,
        )
    }

    /// Build an evaluator from already-emitted wasm bytes against a
    /// caller-supplied [`wasmtime::Engine`]. Cache-aware constructors
    /// route through here so every evaluator built off the same
    /// [`AotCache`] shares a single engine — the change that lifts
    /// cached cold start under 100 μs.
    pub fn from_bytes_with_engine(
        engine: Engine,
        bytes: Vec<u8>,
        main_schema: Schema,
        return_schema: Schema,
    ) -> Result<Self, BuildError> {
        Self::reject_unsupported_fields(&main_schema)?;
        Self::reject_unsupported_fields(&return_schema)?;

        let main_layout = SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| BuildError::LoweringError(format!("main schema layout: {e}")))?;
        let return_layout = SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| BuildError::LoweringError(format!("return schema layout: {e}")))?;

        let module =
            WasmModule::from_bytes(bytes).map_err(|e| BuildError::WasmLoadError(e.to_string()))?;
        let compiled = WtModule::new(&engine, module.bytes())
            .map_err(|e| BuildError::WasmInstantiateError(e.to_string()))?;
        let instance_pre = build_instance_pre(&engine, &compiled)?;

        Ok(Self {
            module,
            engine,
            instance_pre,
            main_schema,
            return_schema,
            main_layout,
            return_layout,
            out_cap: DEFAULT_OUT_CAP,
            // Zero-trust default: every capability bit cleared until
            // the host explicitly grants one through
            // `with_capabilities`. The codegen-emitted check_cap
            // prologue traps with `CapabilityDenied` when a #native
            // call needs a bit the bitmap doesn't carry.
            cap_grants: Capabilities::default().to_cap_bitmap(),
            fuel_limit: 0,
            session_pool: Mutex::new(Vec::new()),
        })
    }

    /// Override the `out_cap` byte budget used for each `run_main`
    /// call. Defaults to 4 KiB; bump it when the return schema's
    /// tail records (strings / list<Int>) can exceed the budget.
    pub fn with_out_cap(mut self, out_cap: u32) -> Self {
        self.out_cap = out_cap;
        self
    }

    /// Replace the capability grant set the wasm side observes through
    /// its internal `relon_caps_avail` i64 global. Defaults to the
    /// zero-trust grant (no bits set); hosts that need to invoke
    /// `#native` functions guarded by a non-zero `cap_bit` must hand
    /// in a [`Capabilities`] that grants every required bit.
    ///
    /// Bit positions follow
    /// [`relon_eval_api::CapabilityBit`] — the same table the
    /// host SDK uses when registering a `#native` function's
    /// `cap_bit` field. Phase 11: the bitmap is forwarded through the
    /// `run_main` argument on every call, so updating
    /// `cap_grants` takes effect on the next call without flushing
    /// the session pool.
    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.cap_grants = caps.to_cap_bitmap();
        self
    }

    /// Phase a-1: cap per-`run_main` wasm execution at `limit` fuel
    /// units (≈ one unit per wasm instruction; `nop` / `drop` /
    /// `block` / `loop` are free — see wasmtime's `Store::set_fuel`
    /// docs for the precise table).
    ///
    /// `limit = 0` disables the budget entirely: the hot path skips
    /// `set_fuel` so non-budgeted hosts keep their previous overhead.
    /// A non-zero value re-stamps the session's store with that fuel
    /// count on every dispatch, so each call starts with a fresh
    /// budget independent of what previous calls consumed (wasmtime
    /// decrements fuel monotonically — without a per-call reset the
    /// second call out of the pool would inherit a near-zero
    /// remainder).
    ///
    /// When the budget runs out wasmtime traps with
    /// [`wasmtime::Trap::OutOfFuel`]; the trap translator maps it to
    /// [`RuntimeError::WasmStepLimitExceeded`]. The `range` field is
    /// best-effort — when the trap's pc lands inside the codegen's
    /// srcmap the original span is attached, otherwise the variant
    /// surfaces with `range = None`.
    ///
    /// The budget is **not** wall-clock time nor cycles: a tight
    /// arithmetic loop and a heavy stdlib call can have wildly
    /// different fuel costs at the same wall-clock budget. Tune
    /// against the entry's expected instruction count, not its
    /// latency.
    pub fn with_fuel_limit(mut self, limit: u64) -> Self {
        self.fuel_limit = limit;
        self
    }

    /// Borrow the configured per-call fuel budget. Hosts that want to
    /// double-check what the evaluator will enforce on the next
    /// `run_main` can read this before dispatching. `0` means
    /// unlimited (the default).
    pub fn fuel_limit(&self) -> u64 {
        self.fuel_limit
    }

    /// Borrow the wrapped [`WasmModule`] — useful for hosts that
    /// want to inspect the parsed `relon.abi` payload or render trap
    /// traces through `module.lookup_pc` outside the trait surface.
    pub fn wasm_module(&self) -> &WasmModule {
        &self.module
    }

    /// Borrow the `#main` schema this evaluator was compiled
    /// against — useful for hosts driving `BufferBuilder` manually.
    pub fn main_schema(&self) -> &Schema {
        &self.main_schema
    }

    /// Borrow the return schema this evaluator was compiled against.
    pub fn return_schema(&self) -> &Schema {
        &self.return_schema
    }

    /// Recursively check that every field of `schema` (and any
    /// nested branded sub-schemas) uses a type the buffer
    /// builder / reader actually supports. Surfaces a precise
    /// [`BuildError::UnsupportedFieldType`] at construction so
    /// the `run_main` hot path doesn't have to defend against it.
    fn reject_unsupported_fields(schema: &Schema) -> Result<(), BuildError> {
        for field in &schema.fields {
            Self::check_type_repr(&field.name, &field.ty)?;
        }
        Ok(())
    }

    fn check_type_repr(field: &str, ty: &TypeRepr) -> Result<(), BuildError> {
        match ty {
            TypeRepr::Int
            | TypeRepr::Float
            | TypeRepr::Bool
            | TypeRepr::Null
            | TypeRepr::String => Ok(()),
            TypeRepr::List { element } => match element.as_ref() {
                TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool | TypeRepr::String => Ok(()),
                TypeRepr::Schema { schema } => Self::reject_unsupported_fields(schema),
                _ => Err(BuildError::UnsupportedFieldType {
                    field: field.to_string(),
                    type_label: "List (unsupported element)",
                }),
            },
            TypeRepr::Schema { schema } => Self::reject_unsupported_fields(schema),
            TypeRepr::Option { .. } => Err(BuildError::UnsupportedFieldType {
                field: field.to_string(),
                type_label: "Option",
            }),
            TypeRepr::Result { .. } => Err(BuildError::UnsupportedFieldType {
                field: field.to_string(),
                type_label: "Result",
            }),
        }
    }

    /// The real `run_main` implementation, shared by the trait
    /// surface and any future host-facing variant that takes an
    /// explicit scope. Builds the `in_buf` from `args`, hands off to a
    /// pooled wasmtime session, then decodes the returned bytes back
    /// into a [`Value`].
    ///
    /// Phase 9.b-1: the per-call session lookup is now a `Mutex` pop
    /// (free-list) instead of a fresh `Store::new` +
    /// `Linker::instantiate`. The pool grows lazily — first call from
    /// each thread / concurrency level builds a session; subsequent
    /// calls on the same level reuse it. The session is always pushed
    /// back even on the error path so a panicking host body doesn't
    /// drain the pool.
    fn run_main_inner(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // Stage 1: build the input buffer. `BufferBuilder::finish`
        // returns the fixed-area + tail-area bytes the wasm side
        // expects at `in_ptr..in_ptr+in_len`.
        let in_bytes = self.build_input(&args)?;

        // Stage 2: borrow a warm session from the pool, or build one
        // on a miss. The miss path pays for `Store::new` +
        // `Linker::instantiate` exactly once per concurrency level.
        let mut session = match self.session_pool.lock() {
            Ok(mut pool) => pool.pop(),
            Err(poisoned) => {
                // Mutex poisoning surfaces only if a previous call
                // panicked while holding the lock. Recover by reading
                // through `into_inner` — the pool's invariants don't
                // span lock acquisitions, so the worst case is one
                // extra session creation.
                let mut pool = poisoned.into_inner();
                pool.pop()
            }
        };

        // Run on a warm session if available, otherwise spin up a new
        // one. The fresh session's `in_cap` is sized to the current
        // call's in_bytes (rounded up to a 64-byte cushion), so most
        // subsequent calls reuse the layout without reshuffling.
        let in_len = in_bytes.len() as u32;
        let session = match session.take() {
            Some(s) if s.in_cap >= in_len => s,
            // Either no session at all, or the cached one's `in_cap` is
            // too small for the current call. Build a fresh session
            // sized to the new in_bytes. The old session (if any) is
            // dropped — its memory pages are reclaimed when its Store
            // goes out of scope. A larger pool design could keep both
            // sizes, but the v1 evaluator only cares about steady-state
            // hot-loop costs where in_len converges fast.
            _ => self.build_session(in_len)?,
        };

        let (result, session) = self.run_main_on_session(session, &in_bytes, in_len);

        // Push the session back even when the wasm body trapped — the
        // store / instance / memory are still valid; only the host-
        // visible `Value` failed to materialise. Dropping a session on
        // trap would force a Store rebuild on the next call and undo
        // the whole point of the pool.
        self.return_session(session);

        result
    }

    /// Build a wasmtime session sized to fit at least `in_len` bytes
    /// of input. The const-data section + leading padding pin
    /// `in_ptr`; the input region runs from `in_ptr..in_ptr +
    /// in_cap`; the output region follows aligned to 8 with capacity
    /// `self.out_cap`. Memory is pre-grown to fit all three so the
    /// hot path skips `memory.grow`.
    ///
    /// Phase 11: the shared `InstancePre` on the evaluator owns the
    /// linker-resolved bindings, so building a session is just
    /// `Store::new` + `InstancePre::instantiate` — the linker's
    /// `define` / validate work is amortised across every store the
    /// evaluator ever opens.
    fn build_session(&self, in_len: u32) -> Result<WasmSession, RuntimeError> {
        let mut store: Store<()> = Store::new(&self.engine, ());

        let instance = self
            .instance_pre
            .instantiate(&mut store)
            .map_err(|e| self.module.translate_trap(&e))?;
        let memory: Memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            RuntimeError::IoError("wasm module missing `memory` export".to_string())
        })?;
        let run_main: TypedFunc<(i32, i32, i32, i32, i64), i32> = instance
            .get_typed_func(&mut store, "run_main")
            .map_err(|e| RuntimeError::IoError(format!("wasm `run_main` export: {e}")))?;

        // Anchor the input buffer past the const-data section. The
        // codegen exports `relon_data_top` set to the byte right after
        // the data section; falling back to `DATA_SECTION_BASE` keeps
        // pre-Phase-3 modules working.
        let data_top = instance
            .get_global(&mut store, "relon_data_top")
            .and_then(|g| match g.get(&mut store) {
                Val::I32(v) => Some(v as u32),
                _ => None,
            })
            .unwrap_or(crate::DATA_SECTION_BASE);

        // Round `in_cap` up to a 64-byte cushion so the next call with
        // a slightly larger input still hits the warm path. The first
        // call paid for the cushion; subsequent ones reuse it.
        const IN_CAP_CUSHION: u32 = 64;
        let in_cap = align_up(in_len.max(IN_CAP_CUSHION), 8);
        let in_ptr = align_up(data_top, 8);
        let out_ptr = align_up(in_ptr + in_cap, 8);

        // Pre-grow the linear memory so the run_main hot path skips
        // the `memory.grow` size-check branch. We size to cover
        // `out_ptr + out_cap` plus one page of slack for any internal
        // tail records the wasm body wants to spill.
        const PAGE_SIZE: usize = 64 * 1024;
        let needed_end = (out_ptr + self.out_cap) as usize + PAGE_SIZE;
        let current_size = memory.data_size(&store);
        if needed_end > current_size {
            let grow_pages = needed_end.div_ceil(PAGE_SIZE) - (current_size / PAGE_SIZE);
            memory
                .grow(&mut store, grow_pages as u64)
                .map_err(|e| RuntimeError::IoError(format!("wasm memory.grow: {e}")))?;
        }

        Ok(WasmSession {
            store,
            memory,
            run_main,
            in_ptr,
            out_ptr,
            in_cap,
        })
    }

    /// Drive `run_main` on a warmed session and return the session
    /// alongside the result. The caller is responsible for pushing the
    /// session back to the pool — the tuple keeps the session live
    /// across the error path so a trap doesn't drain the pool.
    fn run_main_on_session(
        &self,
        mut session: WasmSession,
        in_bytes: &[u8],
        in_len: u32,
    ) -> (Result<Value, RuntimeError>, WasmSession) {
        // Write the input bytes into the wasm linear memory.
        if let Err(e) = session
            .memory
            .write(&mut session.store, session.in_ptr as usize, in_bytes)
        {
            return (
                Err(RuntimeError::IoError(format!(
                    "wasm memory write (in_buf): {e}"
                ))),
                session,
            );
        }

        // Phase a-1: re-stamp the per-call fuel budget. wasmtime
        // decrements fuel monotonically, so without a fresh `set_fuel`
        // the second call out of the pool would inherit whatever the
        // first call left behind. `0` (the default) is the "unlimited"
        // shorthand — skip the call entirely so non-budgeted hosts
        // don't pay the syscall-style cross-call cost on every
        // dispatch.
        if self.fuel_limit > 0 {
            if let Err(e) = session.store.set_fuel(self.fuel_limit) {
                return (
                    Err(RuntimeError::IoError(format!(
                        "wasm set_fuel({}): {e}",
                        self.fuel_limit
                    ))),
                    session,
                );
            }
        }

        let bw = match session.run_main.call(
            &mut session.store,
            (
                session.in_ptr as i32,
                in_len as i32,
                session.out_ptr as i32,
                self.out_cap as i32,
                self.cap_grants as i64,
            ),
        ) {
            Ok(v) => v,
            Err(e) => return (Err(self.module.translate_trap(&e)), session),
        };
        if bw < 0 {
            return (
                Err(RuntimeError::IoError(format!(
                    "wasm run_main reported negative bytes_written: {bw}"
                ))),
                session,
            );
        }
        let bw = bw as usize;
        let mut out_bytes = vec![0u8; bw.max(self.return_layout.root_size)];
        if let Err(e) =
            session
                .memory
                .read(&mut session.store, session.out_ptr as usize, &mut out_bytes)
        {
            return (
                Err(RuntimeError::IoError(format!(
                    "wasm memory read (out_buf): {e}"
                ))),
                session,
            );
        }
        (self.decode_return(&out_bytes), session)
    }

    /// Push a session back on the pool. Silently absorbs `Mutex`
    /// poisoning by clearing the poison flag — losing a session is
    /// not worse than rebuilding one on the next call, and panicking
    /// here would break the user's `run_main` retry loop.
    fn return_session(&self, session: WasmSession) {
        match self.session_pool.lock() {
            Ok(mut pool) => pool.push(session),
            Err(poisoned) => {
                let mut pool = poisoned.into_inner();
                pool.push(session);
            }
        }
    }

    /// Pack `args` into the wasm input buffer using `BufferBuilder`.
    ///
    /// Walks `main_schema.fields` in declaration order so every slot
    /// gets initialised — missing entries trip
    /// [`RuntimeError::MissingMainArg`] before we ever launch the
    /// wasm side (the entry function's `in_len` guard would catch a
    /// short buffer too, but only after a wasm trap, which is
    /// strictly worse diagnostics).
    fn build_input(&self, args: &HashMap<String, Value>) -> Result<Vec<u8>, RuntimeError> {
        let mut builder = BufferBuilder::new(&self.main_layout, &self.main_schema.fields);
        for field in &self.main_schema.fields {
            let value = args
                .get(&field.name)
                .ok_or_else(|| RuntimeError::MissingMainArg {
                    name: field.name.clone(),
                    range: relon_parser::TokenRange::default(),
                })?;
            write_value_into_builder(&mut builder, field, value, &self.main_schema.name)?;
        }
        Ok(builder.finish())
    }

    /// Decode the wasm-emitted return record into a [`Value`].
    ///
    /// When the return schema carries a single `value` field (the
    /// IR-synthesised wrapper for primitive returns), the result is
    /// just that field. When it carries a user schema (the dict /
    /// branded-record path), the entire fixed area is read as a
    /// branded `Value::Dict`.
    fn decode_return(&self, out_bytes: &[u8]) -> Result<Value, RuntimeError> {
        let reader = BufferReader::new(&self.return_layout, &self.return_schema.fields, out_bytes)
            .map_err(buffer_to_runtime_error)?;
        // The IR lowering synthesises a `Ret { value: T }` wrapper for
        // primitive returns. We detect that shape and unwrap it so the
        // host sees the primitive directly. For user-typed returns
        // (named schema), the entire record is the dict.
        if is_single_value_wrapper(&self.return_schema) {
            let field = &self.return_schema.fields[0];
            read_value_from_reader(&reader, field, &self.return_schema)
        } else {
            // Treat as a branded record matching `return_schema.name`.
            let map = read_record_into_map(&reader, &self.return_schema)?;
            Ok(Value::branded_dict(
                map,
                Some(self.return_schema.name.clone()),
            ))
        }
    }
}

/// Lift an [`AotCache`] [`CacheError`] into a [`BuildError::CacheError`]
/// stringification. The cache itself owns its own enum surface; the
/// evaluator's `BuildError` enum exposes a single cache slot so callers
/// don't need to learn two error vocabularies.
fn cache_err(e: CacheError) -> BuildError {
    BuildError::CacheError(e.to_string())
}

/// Phase 11: build a store-independent
/// [`wasmtime::InstancePre`] from the compiled module. The wasm
/// modules emitted by this codegen no longer import any global
/// (`relon_caps_avail` was demoted to a module-internal mutable
/// global), and the evaluator does not bridge `#native` host fns at
/// the SDK layer — the `Linker` therefore stays empty. The resulting
/// pre is cached on the evaluator so every pooled session can stamp
/// itself with a single `InstancePre::instantiate` call without
/// re-paying the validation work.
fn build_instance_pre(engine: &Engine, compiled: &WtModule) -> Result<InstancePre<()>, BuildError> {
    let linker: Linker<()> = Linker::new(engine);
    linker
        .instantiate_pre(compiled)
        .map_err(|e| BuildError::WasmInstantiateError(format!("wasm instance_pre: {e}")))
}

/// Hand a previously-serialised cranelift blob back to wasmtime,
/// skipping the JIT pass.
///
/// The function exists as a tiny dedicated wrapper so the unsafe
/// invocation has exactly one source-level location and one
/// SAFETY comment to audit. Callers must ensure `native_bytes`
/// came from a matching `AotCache::load_native` call — that's the
/// codepath that gates the read on the wasmtime version + target
/// triple stamp recorded in the meta sidecar.
#[allow(unsafe_code)]
fn deserialize_native(engine: &Engine, native_bytes: &[u8]) -> Result<WtModule, String> {
    // SAFETY: `native_bytes` is the verbatim output of
    // `wasmtime::Module::serialize` written by this crate's
    // `AotCache::store_native`. `AotCache::load_native` already
    // rejected any blob whose meta sidecar's `native_compat_hash`
    // disagrees with the current host's wasmtime version + target
    // triple fingerprint, so we never feed `deserialize` cross-
    // version or cross-architecture bytes. wasmtime additionally
    // performs its own version + magic check inside
    // `Module::deserialize` (cf. its docs: "this function is designed
    // to be safe receiving output from *any* compiled version of
    // wasmtime itself"), so a forged blob that slipped past our
    // compat hash would still surface as `Err`, not UB. The unsafe
    // contract we have to uphold is therefore: read bytes only from
    // the cache directory the host configured, and stamp the meta on
    // every write — both invariants live in `cache.rs`.
    let module = unsafe { WtModule::deserialize(engine, native_bytes) }
        .map_err(|e| format!("wasm deserialize: {e}"))?;
    Ok(module)
}

/// Combine the main and return schemas into a single 32-byte schema
/// fingerprint stored in the AOT cache meta. We sha256 the
/// concatenation of the two per-schema canonical hashes so the cache
/// can detect drift on either side without needing two hash slots.
fn combined_schema_hash(main: &Schema, return_: &Schema) -> [u8; 32] {
    use relon_eval_api::schema_canonical::schema_hash;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(schema_hash(main));
    hasher.update(schema_hash(return_));
    hasher.finalize().into()
}

/// Round `value` up to the next multiple of `align`. `align` is
/// expected to be a power of two; callers pass `8` for the
/// in_buf / out_buf placement, which dwarfs every leaf alignment
/// the v1 layout asks for.
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

/// Write `value` into the matching slot of `builder`. Surface a
/// `MainArgTypeMismatch` when the caller-supplied [`Value`] doesn't
/// shape-match the schema's declared type.
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
        // slot without forcing the caller to spell `1.0`. Matches the
        // tree-walker's existing leniency at the host boundary.
        (TypeRepr::Float, Value::Int(v)) => builder
            .write_float(&field.name, *v as f64)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Bool, Value::Bool(v)) => builder
            .write_bool(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Null, Value::Null) => builder
            .write_null(&field.name)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::String, Value::String(s)) => builder
            .write_string(&field.name, s.as_str())
            .map_err(buffer_to_runtime_error),
        (TypeRepr::List { element }, Value::List(items)) => {
            match element.as_ref() {
                TypeRepr::Int => {
                    let mut ints: Vec<i64> = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        match item {
                            Value::Int(n) => ints.push(*n),
                            other => {
                                return Err(RuntimeError::MainArgTypeMismatch {
                                    name: format!("{}[{}]", field.name, i),
                                    expected: "Int".to_string(),
                                    found: other.type_name().to_string(),
                                    range: relon_parser::TokenRange::default(),
                                });
                            }
                        }
                    }
                    builder
                        .write_list_int(&field.name, &ints)
                        .map_err(buffer_to_runtime_error)
                }
                TypeRepr::Float => {
                    let mut floats: Vec<f64> = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        match item {
                            Value::Float(f) => floats.push(f.into_inner()),
                            Value::Int(n) => floats.push(*n as f64),
                            other => {
                                return Err(RuntimeError::MainArgTypeMismatch {
                                    name: format!("{}[{}]", field.name, i),
                                    expected: "Float".to_string(),
                                    found: other.type_name().to_string(),
                                    range: relon_parser::TokenRange::default(),
                                });
                            }
                        }
                    }
                    builder
                        .write_list_float(&field.name, &floats)
                        .map_err(buffer_to_runtime_error)
                }
                TypeRepr::Bool => {
                    let mut bools: Vec<bool> = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        match item {
                            Value::Bool(b) => bools.push(*b),
                            other => {
                                return Err(RuntimeError::MainArgTypeMismatch {
                                    name: format!("{}[{}]", field.name, i),
                                    expected: "Bool".to_string(),
                                    found: other.type_name().to_string(),
                                    range: relon_parser::TokenRange::default(),
                                });
                            }
                        }
                    }
                    builder
                        .write_list_bool(&field.name, &bools)
                        .map_err(buffer_to_runtime_error)
                }
                TypeRepr::String => {
                    let mut strs: Vec<&str> = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        match item {
                            Value::String(s) => strs.push(s.as_str()),
                            other => {
                                return Err(RuntimeError::MainArgTypeMismatch {
                                    name: format!("{}[{}]", field.name, i),
                                    expected: "String".to_string(),
                                    found: other.type_name().to_string(),
                                    range: relon_parser::TokenRange::default(),
                                });
                            }
                        }
                    }
                    builder
                        .write_list_string(&field.name, &strs)
                        .map_err(buffer_to_runtime_error)
                }
                TypeRepr::Schema { schema: sub_schema } => {
                    let sub_layout = SchemaLayout::offsets_for(sub_schema).map_err(|e| {
                        RuntimeError::IoError(format!(
                            "wasm list-sub-record layout for `{field}`: {e}",
                            field = field.name,
                        ))
                    })?;
                    // Borrow the sub_schema reference for the writer's
                    // lifetime — `list_record_writer` requires both the
                    // layout and the schema. Wrap the per-entry write
                    // loop in a closure to keep the per-iteration error
                    // path clean.
                    let mut writer = builder
                        .list_record_writer(&field.name, &sub_layout, sub_schema.as_ref())
                        .map_err(buffer_to_runtime_error)?;
                    for (i, item) in items.iter().enumerate() {
                        let dict = match item {
                            Value::Dict(d) => d,
                            other => {
                                return Err(RuntimeError::MainArgTypeMismatch {
                                    name: format!("{}[{}]", field.name, i),
                                    expected: format!("Dict (schema `{}`)", sub_schema.name),
                                    found: other.type_name().to_string(),
                                    range: relon_parser::TokenRange::default(),
                                });
                            }
                        };
                        let mut child = writer.start_entry();
                        for sub_field in &sub_schema.fields {
                            let sub_value = dict.map.get(&sub_field.name).ok_or_else(|| {
                                RuntimeError::MissingMainArg {
                                    name: format!("{}[{}].{}", field.name, i, sub_field.name),
                                    range: relon_parser::TokenRange::default(),
                                }
                            })?;
                            write_value_into_builder(
                                &mut child,
                                sub_field,
                                sub_value,
                                &sub_schema.name,
                            )?;
                        }
                        writer
                            .finish_entry(builder, child)
                            .map_err(buffer_to_runtime_error)?;
                    }
                    builder
                        .finish_list_record(writer)
                        .map_err(buffer_to_runtime_error)
                }
                other => Err(RuntimeError::MainArgTypeMismatch {
                    name: field.name.clone(),
                    expected: format!("List<{other:?}>"),
                    found: "List".to_string(),
                    range: relon_parser::TokenRange::default(),
                }),
            }
        }
        // Nested branded sub-record. Phase 9.b-1: BufferBuilder::
        // sub_record now packs Schema-typed `#main` args by allocating
        // a detached child builder, recursively writing every field
        // into it, then committing back through finish_sub_record.
        // The Value side accepts either a plain dict literal or a
        // branded dict — both shapes route through the same field walker.
        (TypeRepr::Schema { schema: sub_schema }, Value::Dict(d)) => {
            let sub_layout = SchemaLayout::offsets_for(sub_schema).map_err(|e| {
                RuntimeError::IoError(format!(
                    "wasm sub-record layout for `{field}`: {e}",
                    field = field.name,
                ))
            })?;
            let mut child = builder
                .sub_record(&field.name, &sub_layout, &sub_schema.fields)
                .map_err(buffer_to_runtime_error)?;
            for sub_field in &sub_schema.fields {
                let sub_value =
                    d.map
                        .get(&sub_field.name)
                        .ok_or_else(|| RuntimeError::MissingMainArg {
                            name: format!("{}.{}", field.name, sub_field.name),
                            range: relon_parser::TokenRange::default(),
                        })?;
                write_value_into_builder(&mut child, sub_field, sub_value, &sub_schema.name)?;
            }
            builder
                .finish_sub_record(&field.name, child)
                .map_err(buffer_to_runtime_error)
        }
        (TypeRepr::Schema { .. }, found) => Err(RuntimeError::MainArgTypeMismatch {
            name: field.name.clone(),
            expected: format!("Dict (schema `{schema_name}`)"),
            found: found.type_name().to_string(),
            range: relon_parser::TokenRange::default(),
        }),
        (expected, found) => Err(RuntimeError::MainArgTypeMismatch {
            name: field.name.clone(),
            expected: type_label(expected).to_string(),
            found: found.type_name().to_string(),
            range: relon_parser::TokenRange::default(),
        }),
    }
}

/// Read a single field out of `reader` as a [`Value`]. Recurses into
/// nested branded sub-records so the dict-return path resolves all the
/// way down without a separate driver.
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
            .map(|f| Value::Float(ordered_float::OrderedFloat(f)))
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
                .map(|v| Value::list(v.into_iter().map(Value::Int).collect()))
                .map_err(buffer_to_runtime_error),
            TypeRepr::Float => reader
                .read_list_float(&field.name)
                .map(|v| {
                    Value::list(
                        v.into_iter()
                            .map(|f| Value::Float(ordered_float::OrderedFloat(f)))
                            .collect(),
                    )
                })
                .map_err(buffer_to_runtime_error),
            TypeRepr::Bool => reader
                .read_list_bool(&field.name)
                .map(|v| Value::list(v.into_iter().map(Value::Bool).collect()))
                .map_err(buffer_to_runtime_error),
            TypeRepr::String => reader
                .read_list_string(&field.name)
                .map(|v| {
                    Value::list(v.into_iter().map(|s| Value::String(s.to_string())).collect())
                })
                .map_err(buffer_to_runtime_error),
            TypeRepr::Schema { schema: elem_schema } => {
                let elem_layout = SchemaLayout::offsets_for(elem_schema).map_err(|e| {
                    RuntimeError::IoError(format!("wasm list-sub-record layout: {e}"))
                })?;
                let entries = reader
                    .read_list_record(&field.name, &elem_layout, elem_schema.as_ref())
                    .map_err(buffer_to_runtime_error)?;
                let mut out: Vec<Value> = Vec::with_capacity(entries.len());
                for sub in &entries {
                    let map = read_record_into_map(sub, elem_schema)?;
                    out.push(Value::branded_dict(map, Some(elem_schema.name.clone())));
                }
                Ok(Value::list(out))
            }
            other => Err(RuntimeError::Unsupported {
                reason: format!(
                    "wasm-aot backend cannot read field `{field}` with list element `{ty:?}`",
                    field = field.name,
                    ty = other,
                ),
            }),
        },
        TypeRepr::Schema { schema } => {
            // Borrow the sub-record reader anchored at the parent's
            // pointer slot, then walk every field of the nested schema.
            let sub_layout = SchemaLayout::offsets_for(schema)
                .map_err(|e| RuntimeError::IoError(format!("wasm sub-record layout: {e}")))?;
            let sub_reader = reader
                .sub_record(&field.name, &sub_layout, &schema.fields)
                .map_err(buffer_to_runtime_error)?;
            let map = read_record_into_map(&sub_reader, schema)?;
            Ok(Value::branded_dict(map, Some(schema.name.clone())))
        }
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "wasm-aot backend cannot read field `{field}` of type `{ty:?}` in schema `{schema}`",
                field = field.name,
                ty = other,
                schema = parent_schema.name,
            ),
        }),
    }
}

/// Drain every field of `schema` into a sorted `BTreeMap<String,
/// Value>`. The `BTreeMap` matches the [`relon_eval_api::ValueDict`]
/// inner shape so the caller can wrap the result with
/// `Value::branded_dict` without resorting.
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

/// Map a [`BufferError`] back into a [`RuntimeError`] so the
/// trait surface stays uniform. Buffer-side mismatches always
/// indicate an ABI / schema-drift bug and surface as
/// [`RuntimeError::IoError`] with a descriptive prefix.
fn buffer_to_runtime_error(e: BufferError) -> RuntimeError {
    RuntimeError::IoError(format!("wasm buffer: {e}"))
}

/// Map a [`TypeRepr`] to its human-readable label.
fn type_label(ty: &TypeRepr) -> &'static str {
    match ty {
        TypeRepr::Null => "Null",
        TypeRepr::Bool => "Bool",
        TypeRepr::Int => "Int",
        TypeRepr::Float => "Float",
        TypeRepr::String => "String",
        TypeRepr::List { .. } => "List",
        TypeRepr::Option { .. } => "Option",
        TypeRepr::Result { .. } => "Result",
        TypeRepr::Schema { .. } => "Schema",
    }
}

impl Evaluator for WasmAotEvaluator {
    /// Not supported: wasm-AOT has no AST at runtime.
    fn eval(&self, _node: &Node, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend does not support arbitrary node evaluation".to_string(),
        })
    }

    /// Not supported: wasm-AOT compiles the document into `run_main`;
    /// the document's body is no longer reachable as an AST.
    fn eval_root(&self, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend does not support eval_root (no AST at runtime)".to_string(),
        })
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        self.run_main_inner(args)
    }

    /// Not supported: wasm-AOT topologically schedules every binding
    /// at compile time, so there are no live thunks at runtime.
    fn force_thunk(&self, _thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend has no live thunks (topo-eager evaluation)".to_string(),
        })
    }

    /// Not supported: wasm-AOT does not surface closures as
    /// first-class values; user `fn` declarations lower to
    /// wasm-function calls, not [`Value::Closure`].
    fn invoke_closure(
        &self,
        _closure: &relon_eval_api::ClosureData,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend does not expose first-class closures".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_handles_already_aligned() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(16, 8), 16);
    }

    #[test]
    fn align_up_rounds_up() {
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(7, 8), 8);
        assert_eq!(align_up(9, 8), 16);
    }

    /// Phase 9.c-2: two evaluators built off the same [`AotCache`]
    /// must share a single wasmtime [`Engine`]. Catching engine drift
    /// here is the lightweight guard that lets us drop the per-call
    /// `Engine::default()` from the cached cold-start budget — if a
    /// future refactor accidentally re-introduces a per-evaluator
    /// engine the bench numbers regress silently, but `Engine::same`
    /// gives us a deterministic test signal.
    #[test]
    fn cache_engine_is_shared_across_evaluators() {
        use crate::cache::AotCache;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        // Distinct temp dir per run so concurrent test invocations do
        // not stomp each other's cache state.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "relon-aot-engine-pool-test-{pid}-{nanos}-{counter}"
        ));
        let cache = AotCache::open(&dir).expect("open cache");

        let src = "#main(Int x, Int y) -> Int\nx + y";
        let a = WasmAotEvaluator::from_source_with_cache(src, &cache)
            .expect("first build (miss → store)");
        let b = WasmAotEvaluator::from_source_with_cache(src, &cache)
            .expect("second build (hit + deserialize)");

        assert!(
            Engine::same(&a.engine, &b.engine),
            "evaluators built off the same cache must share an Engine"
        );
        assert!(
            Engine::same(cache.engine(), &a.engine),
            "evaluator engine must match cache.engine()"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two `from_source` calls (no cache) must reuse the
    /// process-wide [`shared_default_engine`] so the non-cache path
    /// also avoids paying `Engine::default()` repeatedly.
    #[test]
    fn shared_engine_reused_across_from_source() {
        let src = "#main(Int x) -> Int\nx + 1";
        let a = WasmAotEvaluator::from_source(src).expect("first build");
        let b = WasmAotEvaluator::from_source(src).expect("second build");
        assert!(
            Engine::same(&a.engine, &b.engine),
            "from_source must reuse the process-wide shared Engine"
        );
        assert!(
            Engine::same(shared_default_engine(), &a.engine),
            "from_source engine must match shared_default_engine()"
        );
    }

    /// Phase 11: the evaluator builds a single `InstancePre` at
    /// construction time and reuses it across every pooled session
    /// the pool ever opens. Driving many `run_main` calls back-to-back
    /// must surface the same answer the first call produced; the new
    /// instantiate path (`InstancePre::instantiate` instead of
    /// `Linker::instantiate`) is otherwise transparent to the host.
    /// The capability bitmap is now routed through the trailing i64
    /// argument of `run_main`, so the per-call grant set takes effect
    /// without flushing the session pool.
    #[test]
    fn instance_pre_reused_across_many_run_main_calls() {
        let src = "#main(Int x) -> Int\nx * 2";
        let aot = WasmAotEvaluator::from_source(src).expect("compile");

        for x in 0..32_i64 {
            let mut args = HashMap::new();
            args.insert("x".to_string(), Value::Int(x));
            let v = aot.run_main(args).expect("run_main");
            match v {
                Value::Int(n) => assert_eq!(n, x * 2, "run_main(x) must return 2*x for x={x}"),
                other => panic!("expected Value::Int, got {other:?}"),
            }
        }

        // The pool should never grow past a single session under
        // serial driving — each call pops the same session and pushes
        // it back. Catching pool growth here would surface a
        // regression where `build_session` is called more than once.
        let pool_len = aot.session_pool.lock().expect("pool lock").len();
        assert_eq!(
            pool_len, 1,
            "single-threaded driving must reuse one pooled session"
        );
    }

    /// Phase 11: `with_capabilities` must take effect on the next
    /// `run_main` call without flushing the session pool — the
    /// capability bitmap is now a per-call argument rather than a
    /// per-session imported global. We pre-warm the pool by running
    /// once with `all_granted`, flip to a zero-trust grant, and expect
    /// the same pooled session to surface the cap-denied trap on the
    /// follow-up call.
    #[test]
    fn capabilities_swap_uses_pooled_session() {
        use relon_eval_api::{Capabilities, CapabilityBit};

        let cap_bit = CapabilityBit::ReadsFs.bit_index();
        let (wasm, main_schema) = super::tests::build_check_cap_module_for_test(cap_bit);
        let return_schema = Schema {
            name: "Ret".into(),
            generics: vec![],
            fields: vec![relon_eval_api::schema_canonical::Field {
                name: "value".into(),
                ty: relon_eval_api::schema_canonical::TypeRepr::Int,
                default: None,
            }],
        };
        let aot = WasmAotEvaluator::from_bytes(wasm, main_schema, return_schema)
            .expect("from_bytes")
            .with_capabilities(Capabilities::all_granted());

        // Warm path: capability granted → the body's constant store
        // reaches the return slot and the session lands in the pool.
        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(0));
        let v = aot.run_main(args.clone()).expect("granted run_main");
        match v {
            Value::Int(n) => assert_eq!(n, 42),
            other => panic!("expected 42, got {other:?}"),
        }
        let pool_len_after_grant = aot.session_pool.lock().expect("pool lock").len();
        assert_eq!(pool_len_after_grant, 1, "granted call must pool a session");

        // Flip to zero-trust without flushing the pool; the next call
        // must trap on the same reused session.
        let aot = aot.with_capabilities(Capabilities::default());
        assert_eq!(
            aot.session_pool.lock().expect("pool lock").len(),
            1,
            "with_capabilities must not drop pooled sessions in Phase 11"
        );
        let err = aot
            .run_main(args)
            .expect_err("zero-trust must trap on cap check");
        match err {
            RuntimeError::WasmCapabilityDenied { cap_bit: bit, .. } => {
                assert_eq!(bit, cap_bit);
            }
            other => panic!("expected WasmCapabilityDenied, got {other:?}"),
        }
    }

    /// Construct a small wasm module guarding the entry body behind a
    /// stand-alone `Op::CheckCap`. Mirrors the helper in
    /// `tests/evaluator_smoke.rs` but lives next to the
    /// `instance_pre_reused_*` / `capabilities_swap_*` unit tests so
    /// they don't need to reach into a sibling integration test.
    pub(super) fn build_check_cap_module_for_test(
        cap_bit: u32,
    ) -> (Vec<u8>, relon_eval_api::schema_canonical::Schema) {
        use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
        use relon_ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
        use relon_parser::TokenRange;

        let synth_range = TokenRange {
            start: relon_parser::TokenPosition {
                line: 1,
                column: 1,
                offset: 0,
            },
            end: relon_parser::TokenPosition {
                line: 1,
                column: 1,
                offset: 0,
            },
        };
        let t = |op: Op| TaggedOp {
            op,
            range: synth_range,
        };
        let main_schema = Schema {
            name: "MainParams".into(),
            generics: vec![],
            fields: vec![Field {
                name: "x".into(),
                ty: TypeRepr::Int,
                default: None,
            }],
        };
        let return_schema = Schema {
            name: "Ret".into(),
            generics: vec![],
            fields: vec![Field {
                name: "value".into(),
                ty: TypeRepr::Int,
                default: None,
            }],
        };
        let return_layout =
            relon_eval_api::layout::SchemaLayout::offsets_for(&return_schema).expect("layout");
        let value_offset = return_layout
            .fields
            .iter()
            .find(|f| f.name == "value")
            .map(|f| f.offset as u32)
            .expect("value offset");

        let ir_module = IrModule {
            imports: vec![],
            funcs: vec![Func {
                name: "run_main".into(),
                params: vec![
                    IrType::I32,
                    IrType::I32,
                    IrType::I32,
                    IrType::I32,
                    IrType::I64,
                ],
                ret: IrType::I32,
                range: synth_range,
                body: vec![
                    t(Op::CheckCap { cap_bit }),
                    t(Op::ConstI64(42)),
                    t(Op::StoreField {
                        offset: value_offset,
                        ty: IrType::I64,
                    }),
                    t(Op::Return),
                ],
            }],
            entry_func_index: Some(0),
            closure_table: vec![],
        };
        let wasm = crate::compile_module(&ir_module, &main_schema, &return_schema)
            .expect("compile check-cap module");
        (wasm, main_schema)
    }
}
