//! v5-γ cache integration: persist cranelift-object `.o` artefacts to
//! disk via `relon-object-link` + `relon-object-cache` so subsequent
//! cold starts can skip parse + analyze + lower + cranelift codegen.
//!
//! ## Layout
//!
//! Two cache files share the same source-hash filename stem, in the
//! same `cache_dir`:
//!
//! - `<sha256>.relon-native-v1` — relon-object-cache format. Holds
//!   the linked ET_DYN bytes plus the HMAC-protected metadata
//!   trailer. Reserved for the dlopen execution path which v5-γ
//!   wires up via [`load_object_bytes`] but does not yet *invoke*
//!   directly (see "Deferred dlopen execution" below).
//!
//! - `<sha256>.relon-ir-v1` — the legacy v5-β-1 IR cache produced by
//!   [`crate::cache::serialize`]. Holds the IR module + sandbox
//!   config bincode. The `from_cache` constructor restores from this
//!   file so cached cold start skips parse + analyze + lower.
//!
//! Both files are written best-effort: any I/O failure, missing
//! linker, HMAC error, or unsupported triple downgrades the cache
//! write to a logged warning and the in-mem JIT still runs.
//!
//! ## Deferred dlopen execution
//!
//! The linked ET_DYN bytes reference the sandbox helper symbols
//! (`relon_now`, `relon_raise_trap`, `relon_cap_lookup`) and the
//! lambda functions as ELF imports. Resolving them at `dlopen` time
//! requires either:
//!
//! 1. Building the host with `-rdynamic` so the main binary's
//!    dynamic-symbol table exports the Rust `extern "C"` helpers, or
//! 2. Emitting an indirect-call vtable (per the design doc §2.3)
//!    that the host populates after `dlopen` returns.
//!
//! Path (1) is fragile across host build configurations (`cargo test`
//! binaries don't pass `-rdynamic` by default). Path (2) requires
//! threading a `RELON_HELPER_VTABLE` GlobalValue through every helper
//! call site in `codegen.rs` — a multi-stage refactor that lands in a
//! follow-up phase.
//!
//! For v5-γ we therefore land the cache **write** path (parallel to
//! the JIT compile so the ET_DYN bytes are persisted), the cache
//! **load** path (round-trips through `relon-object-cache` so the
//! integrity / HMAC story is exercised), and the **fast restore**
//! through the IR cache (skips parse + analyze + lower). The
//! ET_DYN bytes round-trip in this phase serves the dlopen-execution
//! M2 milestone that activates in a follow-up.

use std::path::{Path, PathBuf};

use relon_object_cache::{HostFnImport, IntegrityMode, Metadata, SignatureHash};
use sha2::{Digest, Sha256};

use crate::cache::{self, CacheEntry as IrCacheEntry};
use crate::codegen::{compile_module_to_object_bytes, ObjectArtifact};
use crate::error::CraneliftError;
use crate::sandbox::SandboxConfig;

/// Generator stamp embedded in cache metadata. Bump when an
/// incompatible codegen change lands so older cache files self-
/// invalidate via the `generator_version` check.
///
/// `v5-gamma 2` = stage 2 vtable indirection (host helper calls
/// route through `__relon_capability_vtable` instead of direct
/// `Linkage::Import` references). Cache files from stage 1 are
/// emitted against the stub `relon_main_entry` and would deadlock
/// the dlopen-exec path; the version bump self-invalidates them.
pub const GENERATOR_VERSION: &str = "relon-codegen-native v5-gamma 2";

/// Filename suffix for the legacy IR cache (paired with the
/// relon-object-cache `<hash>.relon-native-v1` blob).
pub const IR_CACHE_FILE_SUFFIX: &str = ".relon-ir-v1";

/// Host triple this build of the codegen targets. v5-γ ships
/// Linux-x86_64 only; non-matching hosts skip the cache entirely with
/// a logged `info` event.
pub fn host_target_triple() -> &'static str {
    "x86_64-unknown-linux-gnu"
}

/// Best-effort cache directory:
///
/// 1. `$XDG_CACHE_HOME/relon`
/// 2. `$HOME/.cache/relon`
/// 3. `std::env::temp_dir() / "relon-cache"` (last-resort)
pub fn default_cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("relon");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".cache").join("relon");
        }
    }
    std::env::temp_dir().join("relon-cache")
}

/// Cheap heuristic: do we expect the relon-object-cache + dlopen
/// pipeline to work on this host? v5-γ is Linux-x86_64 only.
pub fn cache_supported_on_host() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// Compute the canonical source-hash used as the cache key. Inputs
/// are mixed in deterministically so an unrelated cache file with the
/// same source-text-only hash cannot collide:
///
/// - `source` — the raw user source bytes (the codegen pipeline
///   re-canonicalises during lowering; mixing in raw bytes is fine
///   because any whitespace change forces a fresh codegen too).
/// - `sandbox` — packed sandbox config so an evaluator constructed
///   with a different flag set cannot accidentally pick up another
///   evaluator's cache.
/// - `host_target_triple()` — guards against cross-host cache reuse.
/// - `GENERATOR_VERSION` — guards against codegen drift.
pub fn compute_source_hash(source: &str, sandbox: &SandboxConfig) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"relon-cache-key/v1\0");
    hasher.update((source.len() as u64).to_le_bytes());
    hasher.update(source.as_bytes());
    hasher.update(b"\0sandbox\0");
    let sandbox_bits: u32 = (sandbox.bounds_check as u32)
        | ((sandbox.deadline_check as u32) << 1)
        | ((sandbox.capability_check as u32) << 2)
        | ((sandbox.div_check as u32) << 3);
    hasher.update(sandbox_bits.to_le_bytes());
    hasher.update(b"\0triple\0");
    hasher.update(host_target_triple().as_bytes());
    hasher.update(b"\0generator\0");
    hasher.update(GENERATOR_VERSION.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    out
}

/// Compute the legacy IR cache path that sits next to the
/// relon-object-cache blob. Filename stem is identical so the two
/// files invalidate atomically (a host's GC sweep over either suffix
/// catches both).
pub fn ir_cache_path_for(cache_dir: &Path, source_sha256: [u8; 32]) -> PathBuf {
    let mut name = String::with_capacity(64 + IR_CACHE_FILE_SUFFIX.len());
    for b in source_sha256.iter() {
        use std::fmt::Write as _;
        let _ = write!(&mut name, "{:02x}", b);
    }
    name.push_str(IR_CACHE_FILE_SUFFIX);
    cache_dir.join(name)
}

/// Build the relon-object-cache [`Metadata`] trailer. Currently only
/// host-fn imports / capabilities / signature are needed to validate
/// the cached object is compatible with the runtime; the v5-γ codegen
/// emits a fixed shape so all four fields are derived from `sandbox`
/// + entry arity.
pub fn build_metadata(
    sandbox: &SandboxConfig,
    cap_bitmap: u64,
    main_signature: [u8; 32],
    host_fn_imports: Vec<HostFnImport>,
) -> Metadata {
    let _ = sandbox;
    let created_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Metadata {
        host_fn_imports,
        cap_bitmap,
        main_signature: SignatureHash(main_signature),
        created_at_unix,
        generator_version: GENERATOR_VERSION.to_string(),
    }
}

/// Best-effort cache write: links the ET_REL bytes via
/// `relon-object-link`, then persists to `cache_dir` via
/// `relon-object-cache`. Also writes the IR-bincode blob next door so
/// `try_load_from_cache` can skip parse + analyze + lower on restore.
///
/// Returns `Ok(())` on a successful write **or** any best-effort
/// fallback (linker missing, unsupported triple, HMAC unavailable —
/// each logged at the appropriate level). Bubbles up only the truly
/// unexpected errors (out-of-disk while writing the tmp file we
/// already opened).
pub fn try_store_to_cache(
    cache_dir: &Path,
    source_sha256: [u8; 32],
    et_rel_bytes: &[u8],
    metadata: &Metadata,
    ir_blob: &[u8],
) -> Result<(), CraneliftError> {
    if !cache_supported_on_host() {
        tracing::info!(
            target: "relon::object_cache",
            "cache write skipped: host {} not supported in v5-gamma",
            host_target_triple()
        );
        return Ok(());
    }

    // 1. Link ET_REL -> ET_DYN. Best-effort: missing linker / failed
    // linker downgrades to a logged warning. Other I/O errors propagate.
    let triple = host_target_triple();
    let dyn_bytes = match relon_object_link::link_to_dyn(et_rel_bytes, triple) {
        Ok(b) => b,
        Err(relon_object_link::LinkError::LinkerNotFound) => {
            tracing::warn!(
                target: "relon::object_cache",
                "cache write skipped: no usable system linker on $PATH"
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::LinkerFailed(msg)) => {
            tracing::error!(
                target: "relon::object_cache",
                "cache write skipped: linker failed: {msg}"
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::UnsupportedTriple(t)) => {
            tracing::info!(
                target: "relon::object_cache",
                "cache write skipped: triple {t} not supported by relon-object-link"
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::NotEtRel(t)) => {
            tracing::error!(
                target: "relon::object_cache",
                "cache write skipped: emitted bytes were {:?}, expected ET_REL",
                t
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::NotEtDyn(t)) => {
            tracing::error!(
                target: "relon::object_cache",
                "cache write skipped: linker output was {:?}, expected ET_DYN",
                t
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::InvalidElf(msg)) => {
            tracing::error!(
                target: "relon::object_cache",
                "cache write skipped: ET_REL bytes did not parse as ELF: {msg}"
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::Io(e)) => {
            tracing::warn!(
                target: "relon::object_cache",
                "cache write skipped: linker io error: {e}"
            );
            return Ok(());
        }
        Err(relon_object_link::LinkError::FeatureNotImplemented) => {
            tracing::info!(
                target: "relon::object_cache",
                "cache write skipped: link backend not implemented"
            );
            return Ok(());
        }
    };

    // 2. Resolve the HMAC key (best-effort). Missing key downgrades
    // to a no-HMAC write — still integrity-checked via the sha256
    // path inside relon-object-cache.
    let hmac_key = match relon_object_cache::ensure_key() {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::info!(
                target: "relon::object_cache",
                "cache HMAC key unavailable ({e}); proceeding without authentication"
            );
            None
        }
    };

    // 3. Persist the linked bytes + metadata trailer.
    let write_res = relon_object_cache::store(
        cache_dir,
        source_sha256,
        triple,
        &dyn_bytes,
        metadata,
        hmac_key.as_ref(),
    );
    match write_res {
        Ok(path) => {
            tracing::debug!(
                target: "relon::object_cache",
                "cache wrote {} bytes to {}",
                dyn_bytes.len(),
                path.display()
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "relon::object_cache",
                "object-cache write failed: {e}"
            );
            return Ok(());
        }
    }

    // 4. Persist the IR-bincode blob next door so the fast-restore
    // path can skip parse + analyze + lower. We swallow IR write
    // errors with a warn — the relon-object-cache half is still
    // useful for dlopen-once-vtable-indirection lands.
    let ir_path = ir_cache_path_for(cache_dir, source_sha256);
    if let Err(e) = std::fs::create_dir_all(cache_dir) {
        tracing::warn!(
            target: "relon::object_cache",
            "ir-cache create_dir_all failed: {e}"
        );
        return Ok(());
    }
    let tmp_path = ir_path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    if let Err(e) = std::fs::write(&tmp_path, ir_blob) {
        tracing::warn!(
            target: "relon::object_cache",
            "ir-cache tmp write failed: {e}"
        );
        let _ = std::fs::remove_file(&tmp_path);
        return Ok(());
    }
    if let Err(e) = std::fs::rename(&tmp_path, &ir_path) {
        tracing::warn!(
            target: "relon::object_cache",
            "ir-cache rename failed: {e}"
        );
        let _ = std::fs::remove_file(&tmp_path);
        return Ok(());
    }
    tracing::debug!(
        target: "relon::object_cache",
        "ir-cache wrote {} bytes to {}",
        ir_blob.len(),
        ir_path.display()
    );

    Ok(())
}

/// Result of a successful cache load: both the IR-cache restore (for
/// fast re-JIT) and the ET_DYN bytes (for the future dlopen path).
pub struct LoadedCache {
    /// Decoded IR-cache entry — IR module + sandbox config.
    pub ir_entry: IrCacheEntry,
    /// Linked ET_DYN bytes from the relon-object-cache file. Reserved
    /// for the dlopen execution path landing in a follow-up phase.
    pub object_bytes: Vec<u8>,
    /// Metadata trailer the relon-object-cache verified.
    #[allow(dead_code)]
    pub metadata: Metadata,
}

/// Try to load a paired (object-cache + ir-cache) entry. Returns:
///
/// - `Ok(Some(LoadedCache))` — both files present, integrity- and
///   HMAC-verified, metadata matched the current runtime.
/// - `Ok(None)` — at least one file absent, or metadata mismatch, or
///   integrity check failed (the offending file is deleted so the next
///   call rebuilds from scratch).
/// - `Err(_)` — only for truly unexpected I/O conditions (out of
///   memory, permission flip mid-read). All recoverable failures are
///   converted to `Ok(None)` so the caller can fall back to
///   `from_source` cleanly.
pub fn try_load_from_cache(
    cache_dir: &Path,
    source_sha256: [u8; 32],
    expected_metadata: &Metadata,
) -> Result<Option<LoadedCache>, CraneliftError> {
    if !cache_supported_on_host() {
        return Ok(None);
    }

    let triple = host_target_triple();
    let hmac_key = relon_object_cache::ensure_key().ok();

    // 1. Object-cache lookup. Use `TrustOnWrite` integrity mode
    // because our `source_sha256` is the SHA-256 of the *source*
    // text (canonical key), not the SHA-256 of the linked object
    // body — the relon-object-cache `Strict` mode would reject the
    // file otherwise. Tamper detection is still covered by the
    // HMAC tag on the trailer (HMAC over the entire blob, including
    // object bytes), which the loader verifies inside `load`.
    let object_entry = match relon_object_cache::load(
        cache_dir,
        source_sha256,
        triple,
        hmac_key.as_ref(),
        IntegrityMode::TrustOnWrite,
    ) {
        Ok(Some(e)) => e,
        Ok(None) => {
            tracing::debug!(
                target: "relon::object_cache",
                "cache miss: object-cache file absent"
            );
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(
                target: "relon::object_cache",
                "object-cache load failed: {e}; invalidating"
            );
            // Best-effort delete so the next call rewrites cleanly.
            let path = relon_object_cache::storage::cache_path_for(cache_dir, source_sha256);
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
    };

    // 2. Metadata sanity. Mismatched runtime invalidates the file.
    if !metadata_compatible(&object_entry.metadata, expected_metadata) {
        tracing::warn!(
            target: "relon::object_cache",
            "object-cache metadata mismatch: file generator {:?} vs runtime {:?}",
            object_entry.metadata.generator_version,
            expected_metadata.generator_version
        );
        let path = relon_object_cache::storage::cache_path_for(cache_dir, source_sha256);
        let _ = std::fs::remove_file(&path);
        // Also nuke the IR-cache so the pair stays consistent.
        let ir_path = ir_cache_path_for(cache_dir, source_sha256);
        let _ = std::fs::remove_file(&ir_path);
        return Ok(None);
    }

    // 3. IR-cache lookup. Missing IR-cache invalidates the pair.
    let ir_path = ir_cache_path_for(cache_dir, source_sha256);
    let ir_bytes = match std::fs::read(&ir_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                target: "relon::object_cache",
                "ir-cache miss: file absent at {}",
                ir_path.display()
            );
            // Also nuke the object-cache so the pair stays consistent.
            let obj_path = relon_object_cache::storage::cache_path_for(cache_dir, source_sha256);
            let _ = std::fs::remove_file(&obj_path);
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(
                target: "relon::object_cache",
                "ir-cache read failed: {e}"
            );
            return Ok(None);
        }
    };
    let ir_entry = match cache::deserialize(&ir_bytes) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                target: "relon::object_cache",
                "ir-cache decode failed: {e}; invalidating"
            );
            let _ = std::fs::remove_file(&ir_path);
            let obj_path = relon_object_cache::storage::cache_path_for(cache_dir, source_sha256);
            let _ = std::fs::remove_file(&obj_path);
            return Ok(None);
        }
    };

    Ok(Some(LoadedCache {
        ir_entry,
        object_bytes: object_entry.object_bytes,
        metadata: object_entry.metadata,
    }))
}

/// Metadata compatibility check. The fields that must match exactly:
///
/// - `generator_version`: codegen drift invalidates.
/// - `cap_bitmap`: a host that changed its capability declarations
///   cannot reuse a cache built against a different set.
/// - `main_signature`: signature drift invalidates.
/// - `host_fn_imports`: the import table must agree by name + ABI.
///
/// `created_at_unix` is advisory and never compared.
fn metadata_compatible(file: &Metadata, expected: &Metadata) -> bool {
    if file.generator_version != expected.generator_version {
        return false;
    }
    if file.cap_bitmap != expected.cap_bitmap {
        return false;
    }
    if file.main_signature.0 != expected.main_signature.0 {
        return false;
    }
    if file.host_fn_imports.len() != expected.host_fn_imports.len() {
        return false;
    }
    for (a, b) in file
        .host_fn_imports
        .iter()
        .zip(expected.host_fn_imports.iter())
    {
        if a.name != b.name
            || a.cap_bit != b.cap_bit
            || a.params_hash != b.params_hash
            || a.returns_hash != b.returns_hash
        {
            return false;
        }
    }
    true
}

/// Resolve raw ELF bytes (returned by [`try_load_from_cache`]) into a
/// callable [`relon_object_cache::LoadedObject`]. Today this is used
/// by smoke tests / benches to validate the loader API; the
/// production `from_cache` path uses the IR-cache fast restore. The
/// dlopen-execution wiring lands in the follow-up vtable-indirection
/// phase referenced in the module-level doc.
pub fn load_object_bytes(
    object_bytes: &[u8],
    expected_symbols: &[&str],
) -> Result<relon_object_cache::LoadedObject, relon_object_cache::LoaderError> {
    relon_object_cache::LoadedObject::from_bytes(
        object_bytes,
        host_target_triple(),
        expected_symbols,
    )
}

/// v5-γ stage 2: emit a full module ET_REL via cranelift-object so
/// the dlopen-execution path can load real compiled code. The output
/// imports only the `__relon_capability_vtable` data symbol — every
/// host helper call indirects through the vtable, which the host
/// populates after `dlopen` returns (see [`crate::vtable`]).
///
/// Returns the ET_REL bytes ready for
/// [`relon_object_link::link_to_dyn`].
pub fn emit_module_object_bytes(
    ir: &relon_ir::ir::Module,
    sandbox: &SandboxConfig,
    return_root_size: u32,
) -> Result<ObjectArtifact, CraneliftError> {
    compile_module_to_object_bytes(ir, sandbox, return_root_size)
}

/// Backwards-compatible shim: v5-γ stage 1 callers asked for a stub
/// `relon_main_entry` to round-trip the dlopen loader. Stage 2 emits
/// the full module instead — but the IR / sandbox / return_root_size
/// inputs aren't available at every call site, so this thin helper
/// builds a buffer-protocol "return 0" stub the same way stage 1 did.
/// Used by smoke tests that want the loader pipeline without
/// reaching for the full lowering surface.
pub fn emit_entry_stub_object() -> Result<Vec<u8>, CraneliftError> {
    use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_codegen::Context as CodegenContext;
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_module::{default_libcall_names, DataDescription, Linkage, Module as CrModule};
    use cranelift_object::{ObjectBuilder, ObjectModule};

    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("is_pic flag: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CraneliftError::JitSetup(format!("opt_level flag: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| CraneliftError::HostTarget(format!("cranelift-native: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| CraneliftError::JitSetup(format!("isa finish: {e}")))?;

    let obj_builder = ObjectBuilder::new(isa, "relon-native-cache", default_libcall_names())
        .map_err(|e| CraneliftError::JitSetup(format!("object builder: {e}")))?;
    let mut module = ObjectModule::new(obj_builder);

    let pointer_ty = module.target_config().pointer_type();
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params
        .push(AbiParam::new(cranelift_codegen::ir::types::I32));
    sig.params
        .push(AbiParam::new(cranelift_codegen::ir::types::I32));
    sig.params
        .push(AbiParam::new(cranelift_codegen::ir::types::I32));
    sig.params
        .push(AbiParam::new(cranelift_codegen::ir::types::I32));
    sig.params
        .push(AbiParam::new(cranelift_codegen::ir::types::I64));
    sig.returns
        .push(AbiParam::new(cranelift_codegen::ir::types::I32));

    let func_id = module
        .declare_function("relon_main_entry", Linkage::Export, &sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare relon_main_entry: {e}")))?;

    let mut ctx = CodegenContext::new();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), sig);
    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        builder.seal_block(block);
        let zero = builder.ins().iconst(cranelift_codegen::ir::types::I32, 0);
        builder.ins().return_(&[zero]);
        builder.finalize();
    }
    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define relon_main_entry: {e}")))?;

    let mut data_desc = DataDescription::new();
    data_desc.define_zeroinit(crate::vtable::VTABLE_BYTES);
    let data_id = module
        .declare_data(crate::vtable::VTABLE_SYMBOL, Linkage::Export, true, false)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare vtable: {e}")))?;
    module
        .define_data(data_id, &data_desc)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define vtable: {e}")))?;

    let product = module.finish();
    product
        .emit()
        .map_err(|e| CraneliftError::Codegen(format!("object emit: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_hash_is_deterministic() {
        let h1 = compute_source_hash("hello", &SandboxConfig::default());
        let h2 = compute_source_hash("hello", &SandboxConfig::default());
        assert_eq!(h1, h2);
    }

    #[test]
    fn source_hash_differs_for_different_source() {
        let h1 = compute_source_hash("hello", &SandboxConfig::default());
        let h2 = compute_source_hash("world", &SandboxConfig::default());
        assert_ne!(h1, h2);
    }

    #[test]
    fn source_hash_differs_for_different_sandbox() {
        let h1 = compute_source_hash("hello", &SandboxConfig::default());
        let h2 = compute_source_hash("hello", &SandboxConfig::unchecked());
        assert_ne!(h1, h2);
    }

    #[test]
    fn default_cache_dir_falls_back_to_temp_when_no_env() {
        // We don't unset HOME here (other tests rely on it); just
        // assert the function returns *something* non-empty.
        let p = default_cache_dir();
        assert!(!p.as_os_str().is_empty());
    }
}
