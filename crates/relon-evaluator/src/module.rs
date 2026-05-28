//! Module resolution pipeline.
//!
//! `@import("path")` does not look up files itself. Instead, it asks each
//! registered [`ModuleResolver`] in order until one returns
//! `Some(ModuleSource)`. The evaluator then parses and evaluates that source
//! once, caching the result by its [`ModuleSource::canonical_id`].
//!
//! Hosts plug in custom resolvers (in-memory modules, registry/URL lookups,
//! sandbox whitelists) via `Context::prepend_module_resolver`.
//!
//! The [`ModuleResolver`] trait and [`ModuleSource`] payload live in
//! `relon-eval-api` so any evaluator backend can share them; the concrete
//! resolvers below (`StdModuleResolver`, `FilesystemModuleResolver`)
//! ship with the tree-walking evaluator because they bake in the in-tree
//! `std_relon/*.relon` source and the sandbox-aware filesystem traversal
//! check.

pub use relon_eval_api::module::{ModuleResolver, ModuleSource};

// Phase G.W11 Phase 2: `RemoteHttpResolver` (along with its `ureq`
// dep) only ships when the `remote-http` cargo feature is enabled.
// The W11 trivial-`#main` CLI build keeps it off so ureq + rustls +
// ring stay out of the binary (~700 KB savings). Hosts that need
// `https://` `#import` fetch enable `--features remote-http`.
#[cfg(all(not(target_arch = "wasm32"), feature = "remote-http"))]
pub use remote_http::{RemoteHttpResolver, RemoteImportCache};

use relon_eval_api::error::RuntimeError;
use relon_eval_api::scope::Scope;
use relon_parser::TokenRange;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolves the built-in `std/...` virtual modules whose source is embedded
/// in the binary via `include_str!`.
pub struct StdModuleResolver;

impl ModuleResolver for StdModuleResolver {
    fn resolve(
        &self,
        path: &str,
        _scope: &Arc<Scope>,
        _range: TokenRange,
    ) -> Result<Option<ModuleSource>, RuntimeError> {
        if !path.starts_with("std/") {
            return Ok(None);
        }
        let source = match path {
            "std/dict" => include_str!("std_relon/dict.relon"),
            "std/is" => include_str!("std_relon/is.relon"),
            "std/math" => include_str!("std_relon/math.relon"),
            "std/list" => include_str!("std_relon/list.relon"),
            "std/string" => include_str!("std_relon/string.relon"),
            "std/value" => include_str!("std_relon/value.relon"),
            other => {
                // Unknown std/* path: do not let the filesystem resolver try
                // to read it from disk (which would silently succeed if a
                // matching file happened to exist next to the host's CWD).
                return Err(RuntimeError::ModuleNotFound(
                    other.to_string(),
                    _range.into(),
                ));
            }
        };
        Ok(Some(ModuleSource {
            canonical_id: path.to_string(),
            source: source.to_string(),
            current_dir: String::new(),
        }))
    }
}

/// Resolves modules by reading from the local filesystem, using
/// `scope.current_dir` as the base for relative paths.
///
/// Default-constructed instances reject every read: callers must opt in to
/// disk access via [`FilesystemModuleResolver::with_root_dir`] (or the
/// equivalent [`FilesystemModuleResolver::trusted`] for unrestricted access).
/// Once a root is set, every requested path is canonicalized and compared
/// against the (canonical) root to block traversal — including via symlinks
/// that escape the root.
#[derive(Default)]
pub struct FilesystemModuleResolver {
    /// Canonicalized root the resolver is allowed to read from. `None` means
    /// "reject everything"; `Some(_)` enables reads constrained to that
    /// subtree. The dedicated [`Self::trusted`] constructor encodes the
    /// "no root, but allow anything" mode used by the trusted `Context`.
    root: Option<PathBuf>,
    /// Bypass the root check entirely. Set only by [`Self::trusted`]; not
    /// reachable from sandboxed code.
    trusted: bool,
}

impl FilesystemModuleResolver {
    /// Permit reads under `root`. The path is canonicalized eagerly so the
    /// allowlist check is just a prefix comparison at resolve time.
    pub fn with_root_dir(path: impl Into<PathBuf>) -> Self {
        let raw = path.into();
        let root = std::fs::canonicalize(&raw).unwrap_or(raw);
        Self {
            root: Some(root),
            trusted: false,
        }
    }

    /// Wide-open resolver — every path is allowed. Used by hosts that
    /// have full FS trust over the running script (CLI, host's own
    /// config files); never use for untrusted scripts. The host must
    /// also flip `Capabilities::reads_fs` for the gate machinery to
    /// agree.
    pub fn trusted() -> Self {
        Self {
            root: None,
            trusted: true,
        }
    }
}

impl ModuleResolver for FilesystemModuleResolver {
    fn resolve(
        &self,
        path: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Option<ModuleSource>, RuntimeError> {
        if !self.trusted && self.root.is_none() {
            if path.starts_with("std/") {
                return Ok(None);
            }
            // Default-reject: no root configured and not trusted.
            return Err(RuntimeError::CapabilityDenied {
                name: format!("#import {path:?}"),
                reason: "filesystem reads disabled (no root configured)".to_string(),
                range,
            });
        }

        let target_path = Path::new(scope.current_dir.as_ref()).join(path);

        // Workspace-aware fallback so workspace-root `cargo test` can
        // resolve fixtures that live under `crates/relon-evaluator/`. Gated
        // to test builds — production hosts should never silently redirect
        // imports based on the process cwd.
        #[cfg(test)]
        let target_path = {
            let mut t = target_path;
            if !t.exists() {
                let fallback = Path::new("crates/relon-evaluator").join(&t);
                if fallback.exists() {
                    t = fallback;
                }
            }
            t
        };

        let canonical_target = match std::fs::canonicalize(&target_path) {
            Ok(path) => path,
            // Fall through to the next resolver (e.g. `StdModuleResolver`)
            // only if the path simply isn't there; surface real I/O
            // errors (permissions, etc.) so hosts don't silently redirect.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(RuntimeError::IoError(format!(
                    "{}: {e}",
                    target_path.to_string_lossy()
                )))
            }
        };

        // Traversal check after canonicalization, so symlinks pointing
        // outside `root` are caught the same way as `../` escapes.
        if let Some(root) = &self.root {
            if !canonical_target.starts_with(root) {
                return Err(RuntimeError::CapabilityDenied {
                    name: format!("#import {path:?}"),
                    reason: format!("path escapes filesystem root {}", root.to_string_lossy()),
                    range,
                });
            }
        }

        let canonical_id = canonical_target.to_string_lossy().to_string();
        let source = std::fs::read_to_string(&canonical_target)
            .map_err(|e| RuntimeError::IoError(e.to_string()))?;
        let current_dir = canonical_target
            .parent()
            .unwrap_or(Path::new("."))
            .to_string_lossy()
            .to_string();
        Ok(Some(ModuleSource {
            canonical_id,
            source,
            current_dir,
        }))
    }
}

/// v3+ a-3 / v3++ b-3 remote `#import` machinery.
///
/// HTTPS fetch resolver for `#import "https://..."`. Gated to non-wasm32
/// targets so the browser playground build of `relon-wasm-bindings` does not link
/// `ureq` (no sockets / TLS / DNS in `wasm32-unknown-unknown`).
///
/// Mount it manually on hosts that want remote modules; the default
/// chains (`StdModuleResolver`, sandboxed / trusted filesystem) ignore
/// `https://` URLs and let this resolver pick them up.
///
/// `path` is treated as an URL when it starts with `https://` or `http://`.
/// `http://` is rejected unless the host explicitly opted in via
/// [`RemoteHttpResolver::allow_insecure`], so the default posture refuses
/// plaintext code fetches over the wire.
///
/// Internally the module wraps a sync `ureq` agent plus a local on-disk
/// cache (`~/.cache/relon/remote_imports/<sha256_hex>.{body,meta}`,
/// configurable by the host). Cache entries carry an mtime-based TTL so
/// repeated runs of the same script do not hit the network on every
/// cold start. The TTL defaults to 24h and is host-overridable.
///
/// v3++ b-3: when a TTL-expired entry still carries server-supplied
/// validators (`ETag` or `Last-Modified`), the resolver issues a
/// conditional `GET` and treats a `304 Not Modified` response as
/// "reuse the cached body and bump TTL" — saving the entire body's
/// bandwidth on the network round-trip.
#[cfg(all(not(target_arch = "wasm32"), feature = "remote-http"))]
mod remote_http {

    use super::{ModuleResolver, ModuleSource};
    use relon_eval_api::error::{RemoteImportDenial, RemoteImportFailure, RuntimeError};
    use relon_eval_api::scope::Scope;
    use relon_parser::TokenRange;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Default cache lifetime for a fetched module before the resolver
    /// re-issues an HTTP request. Expired entries still try a
    /// conditional `GET` first (when validators exist) before paying
    /// for a full body download.
    pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    /// On-disk sidecar metadata that lives next to a cached body. The
    /// schema is the contract for the `.meta` file format and must
    /// stay backwards-compatible with any older entries on disk.
    ///
    /// `body_sha256` is stored so a future integrity audit (or the
    /// hash-pinning path) can verify the cache is not corrupt without
    /// re-downloading.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(super) struct RemoteCacheMeta {
        /// `ETag` header echoed back from the previous response, if any.
        pub etag: Option<String>,
        /// `Last-Modified` header echoed back from the previous
        /// response, if any.
        pub last_modified: Option<String>,
        /// Unix timestamp (seconds since epoch) of the last successful
        /// fetch or revalidation. Drives the TTL gate.
        pub fetched_at: u64,
        /// SHA-256 of the cached body bytes. Lower-case hex.
        pub body_sha256: String,
    }

    /// Local on-disk cache for remote `#import` bodies. Each entry is
    /// keyed by `sha256(url)` and lives in two sibling files
    /// (`<digest>.body` + `<digest>.meta`) so the body bytes stay
    /// raw and the validator metadata can grow without re-encoding
    /// the payload. Reads are cheap; writes are best-effort — a cache
    /// miss after a successful fetch is non-fatal, the next run
    /// simply re-fetches.
    ///
    /// Legacy v3+ a-3 single-file entries (`<digest>.relon`) are
    /// transparently migrated to the new layout on first access:
    /// the body is rewritten as `<digest>.body`, a fresh `.meta`
    /// with no validators is written, and the legacy file is left
    /// alone so a downgrade path stays viable.
    #[derive(Debug, Clone)]
    pub struct RemoteImportCache {
        root: PathBuf,
        ttl: Duration,
    }

    /// Snapshot returned by [`RemoteImportCache::load`]. Lets the
    /// resolver decide between the fast path (TTL hit, no network),
    /// the revalidation path (TTL expired but validators present), and
    /// the cold fetch path (no entry at all).
    pub(super) struct CachedEntry {
        pub body: String,
        pub meta: RemoteCacheMeta,
        /// `true` when the entry is still inside its TTL window.
        pub fresh: bool,
    }

    impl RemoteImportCache {
        /// Construct a cache rooted at `root`. The directory is created
        /// lazily on first write; missing directories at read time
        /// degrade to "no cached entry".
        pub fn new(root: impl Into<PathBuf>) -> Self {
            Self {
                root: root.into(),
                ttl: DEFAULT_CACHE_TTL,
            }
        }

        /// Pick the default cache location: `$XDG_CACHE_HOME/relon/remote_imports`
        /// when set, otherwise `~/.cache/relon/remote_imports`. Falls
        /// back to the OS temp dir if neither env var is usable so the
        /// resolver never panics on a host without a home directory.
        pub fn default_location() -> Self {
            let root = if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
                PathBuf::from(xdg).join("relon").join("remote_imports")
            } else if let Some(home) = std::env::var_os("HOME") {
                PathBuf::from(home)
                    .join(".cache")
                    .join("relon")
                    .join("remote_imports")
            } else {
                std::env::temp_dir().join("relon").join("remote_imports")
            };
            Self::new(root)
        }

        /// Override the freshness window. Entries older than `ttl` are
        /// treated as candidates for conditional revalidation (or a
        /// full re-fetch when no validators are recorded).
        pub fn with_ttl(mut self, ttl: Duration) -> Self {
            self.ttl = ttl;
            self
        }

        /// Hex digest of `sha256(url)`. Stable filename stem for both
        /// `<stem>.body` and `<stem>.meta`.
        fn digest_for(url: &str) -> String {
            let mut h = Sha256::new();
            h.update(url.as_bytes());
            hex::encode(h.finalize())
        }

        /// Path to the body file under the new (v3++ b-3) schema.
        fn body_path(&self, url: &str) -> PathBuf {
            self.root.join(format!("{}.body", Self::digest_for(url)))
        }

        /// Path to the meta sidecar under the new schema.
        fn meta_path(&self, url: &str) -> PathBuf {
            self.root.join(format!("{}.meta", Self::digest_for(url)))
        }

        /// Path to the legacy v3+ a-3 body-only file. Kept around so
        /// existing on-disk caches are not invalidated on upgrade.
        fn legacy_path(&self, url: &str) -> PathBuf {
            self.root.join(format!("{}.relon", Self::digest_for(url)))
        }

        /// Compute SHA-256 (lower-case hex) of a body. Shared with the
        /// hash-pinning code path so legacy migrations get the same
        /// digest as a fresh fetch.
        fn body_digest(body: &str) -> String {
            let mut h = Sha256::new();
            h.update(body.as_bytes());
            hex::encode(h.finalize())
        }

        /// Current unix time in seconds. Clock skew (system clock
        /// moved backwards) is tolerated by saturating to 0.
        fn now_unix() -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        }

        /// Best-effort load of a cached entry. Migrates legacy bodies
        /// in place when needed. Returns `None` when nothing is
        /// cached for `url` or any I/O error short-circuits the load.
        pub(super) fn load(&self, url: &str) -> Option<CachedEntry> {
            let body_path = self.body_path(url);
            let meta_path = self.meta_path(url);

            if body_path.exists() {
                let body = std::fs::read_to_string(&body_path).ok()?;
                let meta = Self::read_meta(&meta_path).unwrap_or_else(|| {
                    // Body without meta: synthesise a meta carrying the
                    // current body digest so future revalidation has
                    // *something* to anchor on (still no validators).
                    self.synth_meta(&body, &meta_path)
                });
                if Self::body_digest(&body) != meta.body_sha256 {
                    // The cache body and sidecar disagree. Treat the
                    // entry as corrupt and remove both halves so a
                    // later resolve cannot keep serving stale bytes or
                    // accept a 304 against the wrong body.
                    let _ = std::fs::remove_file(&body_path);
                    let _ = std::fs::remove_file(&meta_path);
                    return None;
                }
                let fresh = self.is_fresh(meta.fetched_at);
                return Some(CachedEntry { body, meta, fresh });
            }

            // Legacy fallback: a pre-b-3 cache only has the `.relon`
            // file. Migrate to the new layout so subsequent loads are
            // identical to a freshly-written entry.
            let legacy = self.legacy_path(url);
            if legacy.exists() {
                let body = std::fs::read_to_string(&legacy).ok()?;
                // Use the legacy file's mtime as the "fetched_at"
                // anchor so TTL semantics are preserved across the
                // upgrade boundary.
                let fetched_at = std::fs::metadata(&legacy)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or_else(Self::now_unix);
                let meta = RemoteCacheMeta {
                    etag: None,
                    last_modified: None,
                    fetched_at,
                    body_sha256: Self::body_digest(&body),
                };
                self.write_body(&body_path, &body);
                Self::write_meta(&meta_path, &meta);
                let fresh = self.is_fresh(meta.fetched_at);
                return Some(CachedEntry { body, meta, fresh });
            }

            None
        }

        /// `true` when an entry stamped at `fetched_at` is still within
        /// the TTL window. The comparison is strict so a TTL of zero
        /// always re-validates (useful for hosts that want "cache for
        /// 304s only" semantics). Clock-skew safe — a `fetched_at`
        /// that lies in the future saturates to age 0, which still
        /// counts as fresh under any positive TTL.
        fn is_fresh(&self, fetched_at: u64) -> bool {
            let now = Self::now_unix();
            let age = now.saturating_sub(fetched_at);
            Duration::from_secs(age) < self.ttl
        }

        fn read_meta(path: &Path) -> Option<RemoteCacheMeta> {
            let raw = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&raw).ok()
        }

        fn write_meta(path: &Path, meta: &RemoteCacheMeta) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(encoded) = serde_json::to_string(meta) {
                let _ = std::fs::write(path, encoded);
            }
        }

        fn write_body(&self, path: &Path, body: &str) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, body);
        }

        /// Materialise a meta sidecar from a body-only entry. The
        /// returned value is what an in-memory caller sees; the sync
        /// write to disk is best-effort.
        fn synth_meta(&self, body: &str, meta_path: &Path) -> RemoteCacheMeta {
            let meta = RemoteCacheMeta {
                etag: None,
                last_modified: None,
                fetched_at: Self::now_unix(),
                body_sha256: Self::body_digest(body),
            };
            Self::write_meta(meta_path, &meta);
            meta
        }

        /// Persist a freshly-fetched body + validator metadata. Caller
        /// supplies the headers it observed; either or both may be
        /// `None` when the origin omitted them.
        pub(super) fn store(
            &self,
            url: &str,
            body: &str,
            etag: Option<String>,
            last_modified: Option<String>,
        ) {
            let body_path = self.body_path(url);
            let meta_path = self.meta_path(url);
            self.write_body(&body_path, body);
            let meta = RemoteCacheMeta {
                etag,
                last_modified,
                fetched_at: Self::now_unix(),
                body_sha256: Self::body_digest(body),
            };
            Self::write_meta(&meta_path, &meta);
        }

        /// Mark an existing entry as freshly revalidated (HTTP 304
        /// path). Body bytes are not touched; the `fetched_at`
        /// timestamp is bumped and any newly-observed validator is
        /// folded in so subsequent revalidations stay accurate.
        pub(super) fn refresh(
            &self,
            url: &str,
            existing: &RemoteCacheMeta,
            new_etag: Option<String>,
            new_last_modified: Option<String>,
        ) {
            let meta = RemoteCacheMeta {
                etag: new_etag.or_else(|| existing.etag.clone()),
                last_modified: new_last_modified.or_else(|| existing.last_modified.clone()),
                fetched_at: Self::now_unix(),
                body_sha256: existing.body_sha256.clone(),
            };
            Self::write_meta(&self.meta_path(url), &meta);
        }
    }

    /// Resolver entry: detects `https://` (and, with opt-in,
    /// `http://`) URLs, optionally consults a local cache, then issues
    /// a sync HTTPS GET via `ureq`. Failures are surfaced as
    /// `RuntimeError::RemoteImportFailed`; the sandbox gate is enforced
    /// by the caller (the host's resolver chain only mounts this
    /// resolver under `--trust` / `Capabilities::network`).
    pub struct RemoteHttpResolver {
        cache: Option<RemoteImportCache>,
        allow_insecure: bool,
    }

    impl RemoteHttpResolver {
        /// Resolver with the default cache (`~/.cache/relon/remote_imports`,
        /// 24h TTL) and `https://`-only policy.
        pub fn new() -> Self {
            Self {
                cache: Some(RemoteImportCache::default_location()),
                allow_insecure: false,
            }
        }

        /// Resolver with a host-supplied cache (useful for tests and
        /// for hosts that want a per-project cache directory).
        pub fn with_cache(cache: RemoteImportCache) -> Self {
            Self {
                cache: Some(cache),
                allow_insecure: false,
            }
        }

        /// Disable the on-disk cache entirely. Every resolve issues an
        /// HTTP fetch. Useful for tests and for hosts that have their
        /// own caching layer in front of the resolver.
        pub fn without_cache(mut self) -> Self {
            self.cache = None;
            self
        }

        /// Opt in to plaintext `http://` URLs. Off by default so the
        /// default trust posture refuses fetching executable Relon
        /// code over an unencrypted channel.
        pub fn allow_insecure(mut self, allow: bool) -> Self {
            self.allow_insecure = allow;
            self
        }

        /// Best-effort: returns `true` when `path` looks like a URL this
        /// resolver knows how to handle. Used by host-side gating to
        /// emit a clean `RemoteImportDenied` *before* this resolver
        /// gets a chance to fetch.
        pub fn is_url(path: &str) -> bool {
            path.starts_with("https://") || path.starts_with("http://")
        }

        fn fetch(&self, url: &str, range: TokenRange) -> Result<String, RuntimeError> {
            // Fast path: a still-fresh cache entry never touches the
            // network. Stale entries (TTL expired) fall through to
            // the conditional-GET path below so the server can confirm
            // the body is unchanged without re-sending it.
            let cached = self.cache.as_ref().and_then(|c| c.load(url));
            if let Some(entry) = &cached {
                if entry.fresh {
                    return Ok(entry.body.clone());
                }
            }

            // Build the request. When we have a stale entry with at
            // least one validator, ask the origin to short-circuit
            // with `304 Not Modified` instead of resending the body.
            let mut req = ureq::get(url);
            if let Some(entry) = &cached {
                if let Some(etag) = &entry.meta.etag {
                    req = req.header("If-None-Match", etag.as_str());
                }
                if let Some(last_modified) = &entry.meta.last_modified {
                    req = req.header("If-Modified-Since", last_modified.as_str());
                }
            }

            let response = req.call().map_err(|err| RuntimeError::RemoteImportFailed {
                payload: Box::new(RemoteImportFailure {
                    url: url.to_string(),
                    cause: err.to_string(),
                }),
                range,
            })?;

            let status = response.status();

            // 304 Not Modified: the cached body is still authoritative.
            // ureq 3's default `http_status_as_error` only flags 4xx /
            // 5xx, so 304 lands here as a regular Ok response.
            if status.as_u16() == 304 {
                if let (Some(cache), Some(entry)) = (self.cache.as_ref(), cached.as_ref()) {
                    let new_etag = header_string(&response, "ETag");
                    let new_last_modified = header_string(&response, "Last-Modified");
                    cache.refresh(url, &entry.meta, new_etag, new_last_modified);
                    return Ok(entry.body.clone());
                }
                // 304 without a cached body is a server bug — surface
                // it cleanly so the caller does not silently load an
                // empty module.
                return Err(RuntimeError::RemoteImportFailed {
                    payload: Box::new(RemoteImportFailure {
                        url: url.to_string(),
                        cause: "server replied 304 Not Modified but no cached body was available"
                            .to_string(),
                    }),
                    range,
                });
            }

            if !status.is_success() {
                return Err(RuntimeError::RemoteImportFailed {
                    payload: Box::new(RemoteImportFailure {
                        url: url.to_string(),
                        cause: format!("HTTP status {}", status.as_u16()),
                    }),
                    range,
                });
            }

            // 200 path: snapshot validators *before* consuming the
            // body, since `into_body` takes the response by value.
            let new_etag = header_string(&response, "ETag");
            let new_last_modified = header_string(&response, "Last-Modified");

            let body = response.into_body().read_to_string().map_err(|err| {
                RuntimeError::RemoteImportFailed {
                    payload: Box::new(RemoteImportFailure {
                        url: url.to_string(),
                        cause: format!("body read failed: {err}"),
                    }),
                    range,
                }
            })?;

            if let Some(cache) = &self.cache {
                cache.store(url, &body, new_etag, new_last_modified);
            }
            Ok(body)
        }
    }

    impl Default for RemoteHttpResolver {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Pull a header value out of an `http::Response<Body>` as an owned
    /// `String`, dropping anything that is not valid UTF-8. Validator
    /// headers (`ETag`, `Last-Modified`) are spec'd as ASCII so the
    /// lossy conversion is observable only against misconfigured
    /// origins — in which case dropping the header degrades cleanly
    /// to "no validator", and the next revalidation just falls back to
    /// a full fetch.
    fn header_string<B>(response: &ureq::http::Response<B>, name: &str) -> Option<String> {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    impl ModuleResolver for RemoteHttpResolver {
        fn resolve(
            &self,
            path: &str,
            _scope: &Arc<Scope>,
            range: TokenRange,
        ) -> Result<Option<ModuleSource>, RuntimeError> {
            let is_https = path.starts_with("https://");
            let is_http = path.starts_with("http://");
            if !is_https && !is_http {
                return Ok(None);
            }
            if is_http && !self.allow_insecure {
                return Err(RuntimeError::RemoteImportDenied {
                    payload: Box::new(RemoteImportDenial {
                        url: path.to_string(),
                        reason:
                            "plaintext http:// disabled; use https:// or opt in via allow_insecure"
                                .to_string(),
                    }),
                    range,
                });
            }

            let body = self.fetch(path, range)?;
            // Nested `#import "./..."` from inside a remote module has
            // no real working directory. Leaving `current_dir` empty
            // routes such imports back through the resolver chain;
            // hosts that want remote-relative imports can layer a
            // dedicated resolver on top of this one.
            Ok(Some(ModuleSource {
                canonical_id: path.to_string(),
                source: body,
                current_dir: String::new(),
            }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;
        use std::thread;

        /// One-shot test HTTP server. Counts the number of accepted
        /// connections so cache hit / miss can be observed from the
        /// caller side. Returns the URL pointing at `/<path>` plus the
        /// hit counter and a join handle that completes when the
        /// listener is dropped.
        struct MockServer {
            addr: String,
            hits: StdArc<AtomicUsize>,
            _join: thread::JoinHandle<()>,
        }

        impl MockServer {
            fn start(body: &'static str, status: u16) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let addr = listener.local_addr().expect("local_addr").to_string();
                let hits = StdArc::new(AtomicUsize::new(0));
                let hits_clone = hits.clone();
                let join = thread::spawn(move || {
                    for stream in listener.incoming() {
                        let mut stream = match stream {
                            Ok(s) => s,
                            Err(_) => break,
                        };
                        hits_clone.fetch_add(1, Ordering::SeqCst);
                        // Drain request headers so the client can
                        // proceed past write — a one-shot read is
                        // enough; we don't need to parse anything.
                        use std::io::{Read, Write};
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf);
                        let reason = match status {
                            200 => "OK",
                            500 => "Internal Server Error",
                            _ => "Status",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                });
                Self {
                    addr,
                    hits,
                    _join: join,
                }
            }

            fn url(&self, path: &str) -> String {
                format!("http://{}{}", self.addr, path)
            }

            fn hit_count(&self) -> usize {
                self.hits.load(Ordering::SeqCst)
            }
        }

        fn temp_cache_dir() -> PathBuf {
            let mut path = std::env::temp_dir();
            let suffix = format!(
                "relon-remote-import-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            path.push(suffix);
            path
        }

        #[test]
        fn http_rejected_without_insecure_flag() {
            let resolver = RemoteHttpResolver::new().without_cache();
            let scope = Arc::new(Scope::default());
            let err = resolver
                .resolve(
                    "http://example.com/foo.relon",
                    &scope,
                    TokenRange::default(),
                )
                .unwrap_err();
            assert!(matches!(err, RuntimeError::RemoteImportDenied { .. }));
        }

        #[test]
        fn non_url_path_passes_through() {
            let resolver = RemoteHttpResolver::new();
            let scope = Arc::new(Scope::default());
            let r = resolver
                .resolve("./local.relon", &scope, TokenRange::default())
                .expect("non-URL should return Ok(None)");
            assert!(r.is_none());
        }

        #[test]
        fn fetch_then_cache_avoids_refetch() {
            let server = MockServer::start("{ a: 1 }", 200);
            let cache_root = temp_cache_dir();
            let cache = RemoteImportCache::new(&cache_root);
            let resolver = RemoteHttpResolver::with_cache(cache).allow_insecure(true);
            let scope = Arc::new(Scope::default());
            let url = server.url("/foo.relon");

            let first = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("first fetch")
                .expect("Some(source)");
            assert_eq!(first.source, "{ a: 1 }");
            assert_eq!(server.hit_count(), 1);

            let second = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("second fetch")
                .expect("Some(source)");
            assert_eq!(second.source, "{ a: 1 }");
            // Cache should have absorbed the second resolve — the mock
            // server's hit counter stays at 1.
            assert_eq!(server.hit_count(), 1);

            // Cleanup. Best-effort; a leftover dir is harmless.
            let _ = std::fs::remove_dir_all(&cache_root);
        }

        #[test]
        fn fetch_5xx_surfaces_clean_error() {
            let server = MockServer::start("oops", 500);
            let resolver = RemoteHttpResolver::new()
                .without_cache()
                .allow_insecure(true);
            let scope = Arc::new(Scope::default());
            let url = server.url("/broken.relon");
            let err = resolver
                .resolve(&url, &scope, TokenRange::default())
                .unwrap_err();
            match err {
                RuntimeError::RemoteImportFailed { payload, .. } => {
                    assert_eq!(payload.url, url);
                    assert!(payload.cause.contains("500"), "cause was {}", payload.cause);
                }
                other => panic!("unexpected error variant: {other:?}"),
            }
        }

        #[test]
        fn unresolvable_host_surfaces_clean_error() {
            // RFC 6761 reserved: `.invalid` is guaranteed to fail DNS
            // on every conforming resolver, so this test does not
            // depend on the host's actual network.
            let resolver = RemoteHttpResolver::new().without_cache();
            let scope = Arc::new(Scope::default());
            let url = "https://nonexistent.invalid/foo.relon";
            let err = resolver
                .resolve(url, &scope, TokenRange::default())
                .unwrap_err();
            assert!(matches!(err, RuntimeError::RemoteImportFailed { .. }));
        }

        #[test]
        fn cache_path_is_stable_for_url() {
            let cache = RemoteImportCache::new("/tmp/relon-remote-import");
            let p1 = cache.body_path("https://example.com/util.relon");
            let p2 = cache.body_path("https://example.com/util.relon");
            assert_eq!(p1, p2);
            let p3 = cache.body_path("https://example.com/other.relon");
            assert_ne!(p1, p3);
            // The meta sidecar shares the digest stem so a single
            // digest collision audit covers both halves of the entry.
            assert_eq!(
                p1.with_extension("meta"),
                cache.meta_path("https://example.com/util.relon")
            );
        }

        #[test]
        fn cache_body_digest_mismatch_is_cache_miss() {
            let cache_root = temp_cache_dir();
            let cache = RemoteImportCache::new(&cache_root);
            let url = "https://example.com/tampered.relon";

            cache.store(url, "{ value: 1 }", Some("\"v1\"".to_string()), None);
            std::fs::write(cache.body_path(url), "{ value: 2 }").expect("tamper body");

            assert!(
                cache.load(url).is_none(),
                "digest mismatch must invalidate the cache entry"
            );
            assert!(
                !cache.body_path(url).exists(),
                "corrupt body should be removed"
            );
            assert!(
                !cache.meta_path(url).exists(),
                "corrupt sidecar should be removed"
            );

            let _ = std::fs::remove_dir_all(&cache_root);
        }

        // -------- v3++ b-3: conditional GET tests --------
        //
        // The tests below use a scripted in-process HTTP server that
        // peeks at the request headers (`If-None-Match`,
        // `If-Modified-Since`) and emits the configured response
        // (status + optional `ETag` / `Last-Modified` + body). That's
        // enough to drive the conditional-GET decision tree without
        // pulling in `hyper` or `mockito`.

        /// One scripted reply. `body_when_full` is the body served on
        /// 200 responses. 304 responses always have an empty body, as
        /// per RFC 7232 §4.1.
        #[derive(Clone)]
        struct ScriptedReply {
            status: u16,
            etag: Option<&'static str>,
            last_modified: Option<&'static str>,
            body_when_full: &'static str,
            /// When `true`, the server obeys conditional-request
            /// headers: if `If-None-Match` matches `etag` (or
            /// `If-Modified-Since` matches `last_modified`), the
            /// response is rewritten to a 304 with no body.
            honors_conditional: bool,
        }

        struct ScriptedServer {
            addr: String,
            hits: StdArc<AtomicUsize>,
            last_request_headers: StdArc<std::sync::Mutex<Vec<String>>>,
            _join: thread::JoinHandle<()>,
        }

        impl ScriptedServer {
            fn start(replies: Vec<ScriptedReply>) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let addr = listener.local_addr().expect("local_addr").to_string();
                let hits = StdArc::new(AtomicUsize::new(0));
                let hits_clone = hits.clone();
                let last_headers = StdArc::new(std::sync::Mutex::new(Vec::<String>::new()));
                let last_headers_clone = last_headers.clone();
                let join = thread::spawn(move || {
                    for (idx, stream) in listener.incoming().enumerate() {
                        let mut stream = match stream {
                            Ok(s) => s,
                            Err(_) => break,
                        };
                        hits_clone.fetch_add(1, Ordering::SeqCst);
                        use std::io::{Read, Write};
                        let mut buf = [0u8; 2048];
                        let read_len = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..read_len]).to_string();
                        last_headers_clone.lock().unwrap().push(request.clone());

                        let reply = replies.get(idx).cloned().unwrap_or_else(|| {
                            replies.last().cloned().expect("at least one reply")
                        });

                        // Conditional-GET emulation: a server that
                        // honors validators rewrites 200 → 304 when the
                        // client's validator matches the one this reply
                        // would have sent.
                        let mut effective_status = reply.status;
                        if reply.honors_conditional && reply.status == 200 {
                            let if_none_match =
                                extract_header(&request, "if-none-match").map(|s| s.to_string());
                            let if_modified_since = extract_header(&request, "if-modified-since")
                                .map(|s| s.to_string());
                            let etag_matches = matches!(
                                (if_none_match.as_deref(), reply.etag),
                                (Some(c), Some(s)) if c.trim() == s.trim()
                            );
                            let date_matches = matches!(
                                (if_modified_since.as_deref(), reply.last_modified),
                                (Some(c), Some(s)) if c.trim() == s.trim()
                            );
                            if etag_matches || date_matches {
                                effective_status = 304;
                            }
                        }

                        let reason = match effective_status {
                            200 => "OK",
                            304 => "Not Modified",
                            500 => "Internal Server Error",
                            _ => "Status",
                        };
                        let body = if effective_status == 304 {
                            ""
                        } else {
                            reply.body_when_full
                        };
                        let mut header_block = format!("HTTP/1.1 {effective_status} {reason}\r\n");
                        if let Some(etag) = reply.etag {
                            header_block.push_str(&format!("ETag: {etag}\r\n"));
                        }
                        if let Some(lm) = reply.last_modified {
                            header_block.push_str(&format!("Last-Modified: {lm}\r\n"));
                        }
                        // A 304 carries no body, and per RFC 7230
                        // §3.3.2 must not advertise Content-Length
                        // either when the field would imply a body.
                        // Origins in the wild send `Content-Length: 0`
                        // or skip the header — we omit it to keep the
                        // wire short.
                        if effective_status != 304 {
                            header_block.push_str(&format!("Content-Length: {}\r\n", body.len()));
                        }
                        header_block.push_str("Connection: close\r\n\r\n");
                        let mut response_bytes = header_block.into_bytes();
                        if effective_status != 304 {
                            response_bytes.extend_from_slice(body.as_bytes());
                        }
                        let _ = stream.write_all(&response_bytes);
                    }
                });
                Self {
                    addr,
                    hits,
                    last_request_headers: last_headers,
                    _join: join,
                }
            }

            fn url(&self, path: &str) -> String {
                format!("http://{}{}", self.addr, path)
            }

            fn hit_count(&self) -> usize {
                self.hits.load(Ordering::SeqCst)
            }

            fn last_request(&self) -> Option<String> {
                self.last_request_headers.lock().unwrap().last().cloned()
            }
        }

        /// Tiny case-insensitive `header: value` extractor. Returns
        /// the value of the *first* match, trimmed.
        fn extract_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
            for line in request.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case(name) {
                        return Some(v.trim());
                    }
                }
            }
            None
        }

        fn zero_ttl_cache(root: &PathBuf) -> RemoteImportCache {
            // A TTL of zero forces every resolve past the freshness
            // gate so the conditional-GET branch is exercised on the
            // very first follow-up call.
            RemoteImportCache::new(root).with_ttl(Duration::from_secs(0))
        }

        #[test]
        fn conditional_get_304_reuses_cache() {
            let server = ScriptedServer::start(vec![
                ScriptedReply {
                    status: 200,
                    etag: Some("\"abc\""),
                    last_modified: None,
                    body_when_full: "{ value: \"v1\" }",
                    honors_conditional: true,
                },
                // Every subsequent request honors the conditional
                // headers, so the second resolve rewrites itself to
                // 304 once `If-None-Match: "abc"` lands.
                ScriptedReply {
                    status: 200,
                    etag: Some("\"abc\""),
                    last_modified: None,
                    body_when_full: "{ value: \"v1\" }",
                    honors_conditional: true,
                },
            ]);

            let cache_root = temp_cache_dir();
            let cache = zero_ttl_cache(&cache_root);
            let resolver = RemoteHttpResolver::with_cache(cache.clone()).allow_insecure(true);
            let scope = Arc::new(Scope::default());
            let url = server.url("/util.relon");

            let first = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("first fetch should succeed")
                .expect("Some(source)");
            assert_eq!(first.source, "{ value: \"v1\" }");
            assert_eq!(server.hit_count(), 1);

            let second = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("revalidation should succeed")
                .expect("Some(source)");
            assert_eq!(
                second.source, "{ value: \"v1\" }",
                "304 path must serve the cached body verbatim"
            );
            assert_eq!(server.hit_count(), 2, "revalidation still issues a request");
            let last = server.last_request().expect("captured request");
            assert!(
                last.to_ascii_lowercase().contains("if-none-match: \"abc\""),
                "expected If-None-Match in conditional GET, got:\n{last}"
            );

            // The 304 must bump `fetched_at` so the next call is now
            // inside the (zero-second) TTL window only by the slim
            // race margin — practically the meta has been rewritten.
            let entry = cache.load(&url).expect("entry must still be cached");
            assert_eq!(entry.body, "{ value: \"v1\" }");
            assert_eq!(entry.meta.etag.as_deref(), Some("\"abc\""));

            let _ = std::fs::remove_dir_all(&cache_root);
        }

        #[test]
        fn conditional_get_200_replaces_cache() {
            let server = ScriptedServer::start(vec![
                ScriptedReply {
                    status: 200,
                    etag: Some("\"old\""),
                    last_modified: None,
                    body_when_full: "{ value: \"v1\" }",
                    honors_conditional: false,
                },
                ScriptedReply {
                    status: 200,
                    etag: Some("\"new\""),
                    last_modified: None,
                    body_when_full: "{ value: \"v2\" }",
                    honors_conditional: false,
                },
            ]);

            let cache_root = temp_cache_dir();
            let cache = zero_ttl_cache(&cache_root);
            let resolver = RemoteHttpResolver::with_cache(cache.clone()).allow_insecure(true);
            let scope = Arc::new(Scope::default());
            let url = server.url("/util.relon");

            let first = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("first fetch")
                .expect("Some");
            assert_eq!(first.source, "{ value: \"v1\" }");

            let second = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("revalidation hits 200")
                .expect("Some");
            assert_eq!(
                second.source, "{ value: \"v2\" }",
                "200 path must replace the cached body"
            );

            let entry = cache.load(&url).expect("cache must hold the new body");
            assert_eq!(entry.body, "{ value: \"v2\" }");
            assert_eq!(entry.meta.etag.as_deref(), Some("\"new\""));

            let _ = std::fs::remove_dir_all(&cache_root);
        }

        #[test]
        fn conditional_get_no_etag_falls_back_to_last_modified() {
            let server = ScriptedServer::start(vec![
                ScriptedReply {
                    status: 200,
                    etag: None,
                    last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT"),
                    body_when_full: "{ value: \"date-anchored\" }",
                    honors_conditional: true,
                },
                ScriptedReply {
                    status: 200,
                    etag: None,
                    last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT"),
                    body_when_full: "{ value: \"date-anchored\" }",
                    honors_conditional: true,
                },
            ]);

            let cache_root = temp_cache_dir();
            let cache = zero_ttl_cache(&cache_root);
            let resolver = RemoteHttpResolver::with_cache(cache).allow_insecure(true);
            let scope = Arc::new(Scope::default());
            let url = server.url("/util.relon");

            let _ = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("cold fetch");
            let second = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("revalidate")
                .expect("Some");
            assert_eq!(second.source, "{ value: \"date-anchored\" }");

            let last = server.last_request().expect("captured");
            assert!(
                last.to_ascii_lowercase().contains("if-modified-since:"),
                "expected If-Modified-Since fallback when no ETag was seen, got:\n{last}"
            );

            let _ = std::fs::remove_dir_all(&cache_root);
        }

        #[test]
        fn conditional_get_no_validators_does_full_refetch() {
            // No `ETag`, no `Last-Modified` — the resolver cannot ask
            // the origin to short-circuit, so a TTL-expired entry
            // simply falls back to the original a-3 behaviour: full
            // refetch on every miss.
            let server = ScriptedServer::start(vec![
                ScriptedReply {
                    status: 200,
                    etag: None,
                    last_modified: None,
                    body_when_full: "{ value: \"validator-less\" }",
                    honors_conditional: false,
                },
                ScriptedReply {
                    status: 200,
                    etag: None,
                    last_modified: None,
                    body_when_full: "{ value: \"validator-less\" }",
                    honors_conditional: false,
                },
            ]);
            let cache_root = temp_cache_dir();
            let cache = zero_ttl_cache(&cache_root);
            let resolver = RemoteHttpResolver::with_cache(cache).allow_insecure(true);
            let scope = Arc::new(Scope::default());
            let url = server.url("/util.relon");

            let _ = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("first");
            let _ = resolver
                .resolve(&url, &scope, TokenRange::default())
                .expect("second");
            assert_eq!(
                server.hit_count(),
                2,
                "without validators the resolver still issues a full GET each miss"
            );
            let last = server.last_request().expect("request");
            assert!(
                !last.to_ascii_lowercase().contains("if-none-match"),
                "no validator means no conditional headers, got:\n{last}"
            );
            assert!(
                !last.to_ascii_lowercase().contains("if-modified-since"),
                "no validator means no conditional headers, got:\n{last}"
            );

            let _ = std::fs::remove_dir_all(&cache_root);
        }

        #[test]
        fn legacy_cache_format_migrated() {
            // Hand-write the pre-b-3 schema: just `<digest>.relon`,
            // no `.meta` sidecar. The next `load` must observe the
            // body, materialise a `.meta` (with no validators), and
            // leave subsequent resolves wired into the new schema.
            let cache_root = temp_cache_dir();
            std::fs::create_dir_all(&cache_root).expect("mkdir cache root");
            let cache = RemoteImportCache::new(&cache_root);
            let url = "https://example.com/legacy.relon";
            let legacy = cache.legacy_path(url);
            std::fs::write(&legacy, "{ legacy: true }").expect("seed legacy entry");

            let entry = cache.load(url).expect("legacy entry must load");
            assert_eq!(entry.body, "{ legacy: true }");
            assert!(entry.meta.etag.is_none());
            assert!(entry.meta.last_modified.is_none());
            assert!(
                cache.body_path(url).exists(),
                "load() should have rewritten to the new schema"
            );
            assert!(
                cache.meta_path(url).exists(),
                "load() should have synthesised a .meta sidecar"
            );

            // Second load is now a pure new-schema read; the legacy
            // file is left in place (downgrade safety) but no longer
            // consulted.
            let again = cache.load(url).expect("re-load after migration");
            assert_eq!(again.body, "{ legacy: true }");

            let _ = std::fs::remove_dir_all(&cache_root);
        }
    }
}
