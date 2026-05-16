//! Module resolution protocol.
//!
//! `@import("path")` does not look up files itself. Instead, the evaluator
//! asks each registered [`ModuleResolver`] in order until one returns
//! `Some(ModuleSource)`. The evaluator then parses and evaluates that
//! source once, caching the result by its [`ModuleSource::canonical_id`].
//!
//! Concrete resolver implementations (`StdModuleResolver`,
//! `FilesystemModuleResolver`, host-supplied resolvers) live in the
//! backend crate (`relon-evaluator`); the trait + payload type live here
//! so any backend implementing [`crate::Evaluator`] can share them.

use crate::error::RuntimeError;
use crate::scope::Scope;
use relon_parser::TokenRange;
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
