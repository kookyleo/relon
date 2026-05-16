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
