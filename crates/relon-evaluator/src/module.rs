//! Module resolution pipeline.
//!
//! `@import("path")` does not look up files itself. Instead, it asks each
//! registered [`ModuleResolver`] in order until one returns
//! `Some(ModuleSource)`. The evaluator then parses and evaluates that source
//! once, caching the result by its [`ModuleSource::canonical_id`].
//!
//! Hosts plug in custom resolvers (in-memory modules, registry/URL lookups,
//! sandbox whitelists) via [`crate::eval::Context::prepend_module_resolver`].

use crate::error::RuntimeError;
use crate::scope::Scope;
use relon_parser::TokenRange;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The source text plus identity of a module produced by a [`ModuleResolver`].
#[derive(Debug, Clone)]
pub struct ModuleSource {
    /// Stable identity for this module — used as the key for the module cache
    /// and the cycle-detection stack. For filesystem modules this is the
    /// canonical absolute path; for `std/...` modules it is the virtual path
    /// itself; for host-provided modules it can be any unique string.
    pub canonical_id: String,
    /// The Relon source text to parse and evaluate.
    pub source: String,
    /// Working directory used when nested `@import("./relative.relon")` calls
    /// fire from inside this module. Filesystem resolvers normally set this to
    /// the parent of `canonical_id`; in-memory modules can leave it empty.
    pub current_dir: String,
}

/// A pluggable resolver that answers `@import("path")` requests.
///
/// Resolvers are polled in order; the first non-`None` return value is used.
/// Returning `Ok(None)` defers to the next resolver. Returning `Err(_)` aborts
/// the import without consulting later resolvers.
pub trait ModuleResolver: Send + Sync {
    fn resolve(
        &self,
        path: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Option<ModuleSource>, RuntimeError>;
}

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
    /// "no root, but allow anything" mode used by the trusted [`Context`].
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

    /// Wide-open resolver — every path is allowed. Backs [`Context::trusted`]
    /// and the legacy [`Context::new`] semantics; never use for
    /// untrusted scripts.
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
            // Default-reject: no root configured and not trusted.
            return Err(RuntimeError::CapabilityDenied {
                name: format!("@import({path:?})"),
                reason: "filesystem reads disabled (no root configured)".to_string(),
                range,
            });
        }

        let target_path = Path::new(&scope.current_dir).join(path);
        let canonical_target = std::fs::canonicalize(&target_path).map_err(|e| {
            RuntimeError::IoError(format!("{}: {e}", target_path.to_string_lossy()))
        })?;

        // Traversal check after canonicalization, so symlinks pointing
        // outside `root` are caught the same way as `../` escapes.
        if let Some(root) = &self.root {
            if !canonical_target.starts_with(root) {
                return Err(RuntimeError::CapabilityDenied {
                    name: format!("@import({path:?})"),
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
