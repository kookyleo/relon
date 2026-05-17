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
use std::sync::{Arc, Mutex};
use thiserror::Error;
use wasmtime::{
    Engine, Global, GlobalType, Linker, Memory, Module as WtModule, Mutability, Store, TypedFunc,
    Val, ValType,
};

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
/// `Linker::instantiate`.
///
/// Session reuse is safe because:
///
/// * The module's exported `memory` belongs to the session's `Store`;
///   we overwrite the `in_buf` and `out_buf` regions on every call so
///   stale bytes from a previous invocation are harmless.
/// * The const-data section (string literals, list literals) sits
///   below `in_ptr` and is never mutated by the wasm body — see the
///   `relon_data_top` export the codegen pins.
/// * The single imported global (`relon_caps_avail`) is `Mutability::
///   Const` so no per-call reset is needed.
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
    /// per invocation.
    run_main: TypedFunc<(i32, i32, i32, i32), i32>,
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

/// Phase 8 wasm-AOT backend, Phase 9.b-1 pool-of-stores refactor.
///
/// Holds a precompiled wasm module plus the schemas / layouts the host
/// needs to bridge `Value` ↔ binary handshake. Implements
/// [`Evaluator`] with `run_main` as the only real entry point.
///
/// Per-call cost was historically dominated by `Store::new` +
/// `Linker::instantiate` (≈ 40 μs on the bench laptop). Phase 9.b-1
/// folds those into a pooled [`WasmSession`] that the evaluator
/// recycles across invocations. The session is a `Mutex<Vec<...>>`
/// free-list:
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
    /// JIT-compiled module ready for instantiation. One per evaluator;
    /// reused across `run_main` calls.
    compiled: WtModule,
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
    /// Capability grant bitmap published into the wasm module's
    /// imported `relon_caps_avail` i64 global at session-build time.
    /// Bit positions follow [`relon_eval_api::CapabilityBit`]. Defaults
    /// to the zero-trust grant set (`Capabilities::default`); hosts
    /// flip on individual bits through [`Self::with_capabilities`].
    cap_grants: u64,
    /// Free-list of warmed-up [`WasmSession`]s. Per-call cost on a hit
    /// is just the buffer marshal + wasm dispatch; misses pay the
    /// `Store::new` + `Linker::instantiate` price once.
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
                let engine = Engine::default();
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
        let evaluator = Self::from_bytes(bytes, main_schema, return_schema)?;
        // Best-effort: persist the cranelift-emitted native code so the
        // *next* cold start of this same source skips the JIT entirely
        // through `Module::deserialize`. Write failures are silent
        // (read-only mount, transient ENOSPC, …); they leave the
        // existing `.wasm` / `.schemas` sidecars in place, so a
        // subsequent run still hits the partial-hit JIT-then-save path
        // and gets another chance to produce the sidecar.
        if let Ok(native_bytes) = evaluator.compiled.serialize() {
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
        Ok(Self {
            module,
            engine,
            compiled,
            main_schema,
            return_schema,
            main_layout,
            return_layout,
            out_cap: DEFAULT_OUT_CAP,
            cap_grants: Capabilities::default().to_cap_bitmap(),
            session_pool: Mutex::new(Vec::new()),
        })
    }

    /// Compile `src` end-to-end and return a ready-to-call evaluator.
    ///
    /// Pipeline: `parse_document` → `relon_analyzer::analyze` →
    /// `relon_ir::lower_workspace_single` → `compile_lowered_entry` →
    /// `WasmModule::from_bytes` → `wasmtime::Module::new`.
    pub fn from_source(src: &str) -> Result<Self, BuildError> {
        let (lowered, bytes) = Self::compile_source(src)?;
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
    pub fn from_bytes(
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
        let engine = Engine::default();
        let compiled = WtModule::new(&engine, module.bytes())
            .map_err(|e| BuildError::WasmInstantiateError(e.to_string()))?;

        Ok(Self {
            module,
            engine,
            compiled,
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
    /// its imported `relon_caps_avail` i64 global. Defaults to the
    /// zero-trust grant (no bits set); hosts that need to invoke
    /// `#native` functions guarded by a non-zero `cap_bit` must hand
    /// in a [`Capabilities`] that grants every required bit.
    ///
    /// Bit positions follow
    /// [`relon_eval_api::CapabilityBit`] — the same table the
    /// host SDK uses when registering a `#native` function's
    /// `cap_bit` field. Resets the session pool so cached stores
    /// don't keep the previous bitmap alive.
    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.cap_grants = caps.to_cap_bitmap();
        // Drop pooled sessions: each session's `relon_caps_avail`
        // global was built against the old bitmap. Leaving them
        // around would cause the next call to silently keep observing
        // the stale grant set.
        if let Ok(mut pool) = self.session_pool.lock() {
            pool.clear();
        }
        self
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
            TypeRepr::List { element } => {
                if matches!(element.as_ref(), TypeRepr::Int) {
                    Ok(())
                } else {
                    Err(BuildError::UnsupportedFieldType {
                        field: field.to_string(),
                        type_label: "List (non-Int element)",
                    })
                }
            }
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
    fn build_session(&self, in_len: u32) -> Result<WasmSession, RuntimeError> {
        let mut store: Store<()> = Store::new(&self.engine, ());

        // The wasm module imports `(global $relon_caps_avail i64)`.
        // The bitmap published here is the host's grant set encoded
        // through `CapabilityBit`; codegen's `check_cap` prologue
        // tests the relevant bit before every guarded `#native` call
        // and traps with `CapabilityDenied` on a miss. The Global has
        // to be created against this specific store before the linker
        // can own it — that's why we can't share a single Global
        // across the pool's stores.
        let caps_avail = Global::new(
            &mut store,
            GlobalType::new(ValType::I64, Mutability::Const),
            Val::I64(self.cap_grants as i64),
        )
        .map_err(|e| RuntimeError::IoError(format!("wasm caps_avail global: {e}")))?;

        let mut linker: Linker<()> = Linker::new(&self.engine);
        linker
            .define(&mut store, "env", "relon_caps_avail", caps_avail)
            .map_err(|e| RuntimeError::IoError(format!("wasm linker define: {e}")))?;

        let instance = linker
            .instantiate(&mut store, &self.compiled)
            .map_err(|e| self.module.translate_trap(&e))?;
        let memory: Memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            RuntimeError::IoError("wasm module missing `memory` export".to_string())
        })?;
        let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
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

        let bw = match session.run_main.call(
            &mut session.store,
            (
                session.in_ptr as i32,
                in_len as i32,
                session.out_ptr as i32,
                self.out_cap as i32,
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
        (TypeRepr::List { element }, Value::List(items))
            if matches!(element.as_ref(), TypeRepr::Int) =>
        {
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
        TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int) => reader
            .read_list_int(&field.name)
            .map(|v| Value::list(v.into_iter().map(Value::Int).collect()))
            .map_err(buffer_to_runtime_error),
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
}
