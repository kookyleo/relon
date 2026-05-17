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

#[cfg(not(target_arch = "wasm32"))]
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

        let target_path = Path::new(&scope.current_dir).join(path);

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

/// v3+ a-3: HTTPS fetch resolver for `#import "https://..."`. Gated to
/// non-wasm32 targets so the browser playground build of `relon-wasm`
/// does not link `ureq` (no sockets / TLS / DNS in `wasm32-unknown-unknown`).
///
/// Mount it manually on hosts that want remote modules; the default
/// chains (`StdModuleResolver`, sandboxed / trusted filesystem) ignore
/// `https://` URLs and let this resolver pick them up.
///
/// `path` is treated as an URL when it starts with `https://` or `http://`.
/// `http://` is rejected unless the host explicitly opted in via
/// [`RemoteHttpResolver::allow_insecure`], so the default posture refuses
/// plaintext code fetches over the wire.
#[cfg(not(target_arch = "wasm32"))]
mod remote_http {
    //! v3+ a-3 remote `#import` machinery.
    //!
    //! The module wraps a sync `ureq` agent plus a local on-disk cache
    //! (`~/.cache/relon/remote_imports/<sha256_hex>.relon`, configurable
    //! by the host). Cache entries carry an mtime-based TTL so repeated
    //! runs of the same script do not hit the network on every cold
    //! start. The TTL defaults to 24h and is host-overridable.

    use super::{ModuleResolver, ModuleSource};
    use relon_eval_api::error::RuntimeError;
    use relon_eval_api::scope::Scope;
    use relon_parser::TokenRange;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    /// Default cache lifetime for a fetched module before the resolver
    /// re-issues an HTTP request.
    pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    /// Local on-disk cache for remote `#import` bodies. Each entry is
    /// keyed by `sha256(url)` so URLs longer than the OS filename limit
    /// still serialise cleanly. Reads are cheap (a `read_to_string`);
    /// writes are best-effort — a cache miss after a successful fetch
    /// is non-fatal, the next run simply re-fetches.
    #[derive(Debug, Clone)]
    pub struct RemoteImportCache {
        root: PathBuf,
        ttl: Duration,
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
        /// treated as misses and trigger a re-fetch.
        pub fn with_ttl(mut self, ttl: Duration) -> Self {
            self.ttl = ttl;
            self
        }

        /// Filesystem path that would hold `url`'s cached body.
        fn path_for(&self, url: &str) -> PathBuf {
            let mut h = Sha256::new();
            h.update(url.as_bytes());
            let digest = hex::encode(h.finalize());
            self.root.join(format!("{digest}.relon"))
        }

        /// Read a fresh cached body, if any. Returns `None` on missing
        /// file, expired mtime, or any I/O error — callers always have
        /// the option to fall through to the network fetch.
        fn read(&self, url: &str) -> Option<String> {
            let path = self.path_for(url);
            let meta = std::fs::metadata(&path).ok()?;
            if let Ok(mtime) = meta.modified() {
                if let Ok(age) = SystemTime::now().duration_since(mtime) {
                    if age > self.ttl {
                        return None;
                    }
                }
            }
            std::fs::read_to_string(&path).ok()
        }

        /// Store `body` for `url`. Best-effort — directory creation and
        /// write failures are silently swallowed so a read-only cache
        /// directory does not crash the import.
        fn write(&self, url: &str, body: &str) {
            let path = self.path_for(url);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, body);
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
            // Cache hit short-circuits the network entirely. The hash
            // pinning variant (`RemoteImportHashMismatch`) is reserved
            // but not produced yet — see the error enum doc.
            if let Some(cache) = &self.cache {
                if let Some(body) = cache.read(url) {
                    return Ok(body);
                }
            }

            let response = ureq::get(url).call().map_err(|err| {
                RuntimeError::RemoteImportFailed {
                    url: url.to_string(),
                    cause: err.to_string(),
                    range,
                }
            })?;

            let status = response.status();
            if !status.is_success() {
                return Err(RuntimeError::RemoteImportFailed {
                    url: url.to_string(),
                    cause: format!("HTTP status {}", status.as_u16()),
                    range,
                });
            }

            let body = response.into_body().read_to_string().map_err(|err| {
                RuntimeError::RemoteImportFailed {
                    url: url.to_string(),
                    cause: format!("body read failed: {err}"),
                    range,
                }
            })?;

            if let Some(cache) = &self.cache {
                cache.write(url, &body);
            }
            Ok(body)
        }
    }

    impl Default for RemoteHttpResolver {
        fn default() -> Self {
            Self::new()
        }
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
                    url: path.to_string(),
                    reason: "plaintext http:// disabled; use https:// or opt in via allow_insecure"
                        .to_string(),
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
                .resolve("http://example.com/foo.relon", &scope, TokenRange::default())
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
                RuntimeError::RemoteImportFailed { url: u, cause, .. } => {
                    assert_eq!(u, url);
                    assert!(cause.contains("500"), "cause was {cause}");
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
            let p1 = cache.path_for("https://example.com/util.relon");
            let p2 = cache.path_for("https://example.com/util.relon");
            assert_eq!(p1, p2);
            let p3 = cache.path_for("https://example.com/other.relon");
            assert_ne!(p1, p3);
        }
    }
}
