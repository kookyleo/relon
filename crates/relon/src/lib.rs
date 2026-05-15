#![forbid(unsafe_code)]

pub mod projector;

use relon_analyzer::{
    analyze, analyze_entry, AnalyzedTree, Diagnostic, LoadError, LoadedModule, ModuleLoader,
    WorkspaceDiagnostic,
};
use relon_evaluator::module::{FilesystemModuleResolver, ModuleResolver, StdModuleResolver};
use relon_evaluator::{Capabilities, Context, Evaluator, RuntimeError, Scope, Value};
use relon_parser::parse_document;
use relon_parser::TokenRange;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use projector::{JsonProjector, Projector};
pub use relon_analyzer;
pub use relon_evaluator;
pub use relon_parser;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read Relon file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse Relon source: {0}")]
    Parse(String),

    /// One or more analyzer diagnostics at `Error` severity. Reported as a
    /// batch (the whole point of having an analyzer pass) rather than
    /// fail-fast like [`Error::Eval`].
    #[error("analyzer reported {} error(s)", .0.len())]
    Analyze(Vec<Diagnostic>),

    /// Workspace-level analyzer findings (cycles, missing imports,
    /// cross-module schema collisions, parse errors in imported
    /// modules) plus any per-module analyzer errors discovered while
    /// walking the import graph. Distinct from [`Error::Analyze`] so
    /// hosts can decide to render the import-graph errors with a
    /// different layout (e.g. "imported here" labels).
    #[error(
        "workspace analyzer reported {} workspace-level and {} module-level error(s)",
        workspace.len(),
        modules.len()
    )]
    AnalyzeWorkspace {
        workspace: Vec<WorkspaceDiagnostic>,
        modules: Vec<(String, Diagnostic)>,
    },

    #[error(transparent)]
    Eval(#[from] RuntimeError),

    #[error("failed to deserialize Relon value: {0}")]
    Deserialize(#[from] serde_json::Error),

    #[error("failed to convert Relon value to JSON: non-finite float {0}")]
    NonFiniteFloat(f64),

    #[error("failed to convert Relon value to JSON: closures are not supported in JSON output")]
    UnsupportedClosure,

    #[error("failed to convert Relon value to JSON: schemas are not supported in JSON output")]
    UnsupportedSchema,
}

/// Default-sandboxed: filesystem `#import` and capability-gated
/// native fns are denied; only `std/*` imports resolve. Use
/// [`from_str_trusted`] when the script needs the legacy
/// fully-granted environment.
pub fn from_str<T>(source: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = json_from_str(source)?;
    Ok(serde_json::from_value(value)?)
}

/// Trusted variant: grants every capability and allows local
/// filesystem `#import`. Use only on host-owned input.
pub fn from_str_trusted<T>(source: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = json_from_str_trusted(source)?;
    Ok(serde_json::from_value(value)?)
}

/// Default-sandboxed. See [`from_str`] for the trust posture and
/// [`from_file_trusted`] for the legacy variant.
pub fn from_file<T>(path: impl AsRef<Path>) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = json_from_file(path)?;
    Ok(serde_json::from_value(value)?)
}

/// Trusted variant of [`from_file`].
pub fn from_file_trusted<T>(path: impl AsRef<Path>) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = json_from_file_trusted(path)?;
    Ok(serde_json::from_value(value)?)
}

pub fn json_from_str(source: &str) -> Result<serde_json::Value> {
    to_json_value(value_from_str(source)?)
}

pub fn json_from_str_trusted(source: &str) -> Result<serde_json::Value> {
    to_json_value(value_from_str_trusted(source)?)
}

pub fn json_from_file(path: impl AsRef<Path>) -> Result<serde_json::Value> {
    to_json_value(value_from_file(path)?)
}

pub fn json_from_file_trusted(path: impl AsRef<Path>) -> Result<serde_json::Value> {
    to_json_value(value_from_file_trusted(path)?)
}

pub fn value_from_str(source: &str) -> Result<Value> {
    evaluate_source(source, ".", "<memory>", TrustMode::Sandboxed)
}

pub fn value_from_str_trusted(source: &str) -> Result<Value> {
    evaluate_source(source, ".", "<memory>", TrustMode::Trusted)
}

pub fn value_from_file(path: impl AsRef<Path>) -> Result<Value> {
    value_from_file_inner(path, TrustMode::Sandboxed)
}

pub fn value_from_file_trusted(path: impl AsRef<Path>) -> Result<Value> {
    value_from_file_inner(path, TrustMode::Trusted)
}

fn value_from_file_inner(path: impl AsRef<Path>, trust: TrustMode) -> Result<Value> {
    let path = path.as_ref();
    let canonical_path = std::fs::canonicalize(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let source = std::fs::read_to_string(&canonical_path).map_err(|source| Error::Io {
        path: canonical_path.clone(),
        source,
    })?;
    let current_dir = canonical_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_string_lossy()
        .to_string();
    evaluate_source(
        &source,
        current_dir,
        canonical_path.to_string_lossy().to_string(),
        trust,
    )
}

/// Trust posture used by the facade entry points. The default
/// (`Sandboxed`) refuses filesystem `#import` and capability-gated
/// native functions; `Trusted` grants every capability.
#[derive(Debug, Clone, Copy)]
enum TrustMode {
    Sandboxed,
    Trusted,
}

/// Project an evaluated [`Value`] to `serde_json::Value` using the default
/// [`JsonProjector`]. See [`project_with`] when you need to plug in a
/// custom [`Projector`].
pub fn to_json_value(value: Value) -> Result<serde_json::Value> {
    JsonProjector.project(&value)
}

/// Project a [`Value`] using a caller-supplied [`Projector`]. Lifts the
/// projector's error into a `Result<P::Output, P::Error>`, so the host
/// keeps full control over the error type — no detour through
/// [`crate::Error`] required.
pub fn project_with<P: Projector>(
    projector: &P,
    value: &Value,
) -> std::result::Result<P::Output, P::Error> {
    projector.project(value)
}

/// Convenience: parse `source`, evaluate, and project with the supplied
/// projector. Parse / evaluation errors are returned unchanged via
/// [`crate::Error`]; projection errors are surfaced through `P::Error` and
/// must be combinable with [`crate::Error`] by the caller (or use the
/// fixed-format [`from_str`] / [`json_from_str`] helpers).
pub fn project_from_str<P: Projector>(
    source: &str,
    projector: &P,
) -> std::result::Result<P::Output, ProjectError<P::Error>> {
    let value = value_from_str(source).map_err(ProjectError::Eval)?;
    projector.project(&value).map_err(ProjectError::Project)
}

/// Trusted variant of [`project_from_str`].
pub fn project_from_str_trusted<P: Projector>(
    source: &str,
    projector: &P,
) -> std::result::Result<P::Output, ProjectError<P::Error>> {
    let value = value_from_str_trusted(source).map_err(ProjectError::Eval)?;
    projector.project(&value).map_err(ProjectError::Project)
}

/// Combined error type returned by [`project_from_str`]: separates
/// evaluation failures (already typed) from projection failures (whichever
/// error type the projector chose).
#[derive(Debug, thiserror::Error)]
pub enum ProjectError<P> {
    #[error(transparent)]
    Eval(crate::Error),
    #[error("projection failed")]
    Project(#[source] P),
}

/// Adapter that bridges the analyzer's `ModuleLoader` trait to the
/// evaluator's existing `ModuleResolver` chain. Lets the workspace
/// pass reuse exactly the same lookup rules (std/* virtual modules,
/// trusted filesystem) as runtime imports, without the analyzer crate
/// having to depend on `std::fs` directly.
///
/// Binaries that drive the analyzer directly (e.g. `relon-cli`) reuse
/// this through the public [`sandboxed`] / [`trusted`] constructors,
/// so the `ModuleResolver` chain stays defined in one place.
///
/// [`sandboxed`]: ResolverChainLoader::sandboxed
/// [`trusted`]: ResolverChainLoader::trusted
pub struct ResolverChainLoader {
    resolvers: Vec<Arc<dyn ModuleResolver>>,
}

impl ResolverChainLoader {
    /// Sandboxed posture: only `std/*` virtual modules resolve. Local
    /// `#import "./foo.relon"` paths get no resolver and surface as
    /// `ModuleNotFound`, mirroring the default `Capabilities` (no
    /// `reads_fs`).
    pub fn sandboxed() -> Self {
        Self {
            resolvers: vec![Arc::new(StdModuleResolver)],
        }
    }

    /// Trusted posture: `std/*` + trusted filesystem fallback. Any
    /// host change to this chain has to mirror the `Context` assembly
    /// in [`evaluate_source`].
    pub fn trusted() -> Self {
        Self {
            resolvers: vec![
                Arc::new(StdModuleResolver),
                Arc::new(FilesystemModuleResolver::trusted()),
            ],
        }
    }

    /// Custom posture: pass your own resolver chain (for hosts that
    /// install virtual file systems, registry resolvers, etc.).
    pub fn from_resolvers(resolvers: Vec<Arc<dyn ModuleResolver>>) -> Self {
        Self { resolvers }
    }
}

impl ModuleLoader for ResolverChainLoader {
    fn load(
        &mut self,
        path: &str,
        current_dir: &Path,
    ) -> std::result::Result<LoadedModule, LoadError> {
        // The analyzer-side trait is independent of `Scope` — the
        // evaluator-side resolvers want a `Scope` so they can read
        // `current_dir`. Build a synthetic scope that carries just
        // that field, since none of the resolvers we mount in the
        // facade consult any of the others.
        let mut scope = Scope::default();
        scope.current_dir = current_dir.to_string_lossy().to_string();
        let scope = Arc::new(scope);
        for resolver in &self.resolvers {
            match resolver.resolve(path, &scope, TokenRange::default()) {
                Ok(Some(source)) => {
                    let dir = if source.current_dir.is_empty() {
                        current_dir.to_path_buf()
                    } else {
                        PathBuf::from(&source.current_dir)
                    };
                    return Ok(LoadedModule {
                        canonical_id: source.canonical_id,
                        source: source.source,
                        current_dir: dir,
                    });
                }
                Ok(None) => continue,
                Err(RuntimeError::CapabilityDenied { reason, .. }) => {
                    return Err(LoadError::AccessDenied(reason));
                }
                Err(RuntimeError::ModuleNotFound(_, _)) => {
                    return Err(LoadError::NotFound);
                }
                Err(other) => {
                    return Err(LoadError::Other(other.to_string()));
                }
            }
        }
        Err(LoadError::NotFound)
    }
}

/// Reduce a workspace-level error set into the format required by
/// `Error::AnalyzeWorkspace`: workspace-only diagnostics in one bucket,
/// per-module errors in another.
fn workspace_error_payload(
    workspace: &relon_analyzer::WorkspaceTree,
) -> (Vec<WorkspaceDiagnostic>, Vec<(String, Diagnostic)>) {
    let ws_errs: Vec<_> = workspace
        .workspace_diagnostics
        .iter()
        .filter(|d| d.severity() == relon_analyzer::Severity::Error)
        .cloned()
        .collect();
    let mut module_errs: Vec<(String, Diagnostic)> = Vec::new();
    for (id, tree) in &workspace.modules {
        for d in &tree.diagnostics {
            if d.severity() == relon_analyzer::Severity::Error {
                module_errs.push((id.clone(), d.clone()));
            }
        }
    }
    (ws_errs, module_errs)
}

fn evaluate_source(
    source: &str,
    current_dir: impl Into<String>,
    cache_namespace: impl Into<String>,
    trust: TrustMode,
) -> Result<Value> {
    let current_dir = current_dir.into();
    let cache_namespace = cache_namespace.into();

    // Surface entry-level parse failures as `Error::Parse` so callers
    // that want to distinguish "host gave us garbage" from "import
    // graph problem" still can. The workspace pass also catches this,
    // but its `ModuleParseError` shape is targeted at imported modules
    // and includes an "imported here" span that doesn't exist for the
    // entry. We pay the cost of one extra parse on success; cheap.
    parse_document(source).map_err(|err| Error::Parse(err.to_string()))?;

    // Stage 0: drive the analyzer in workspace mode. This pulls the
    // entry plus every transitive `#import`'d module through one BFS,
    // running the per-file analyzer pass on each. Cycles, missing
    // modules, parse / structural errors anywhere in the graph all
    // surface here — before we touch the evaluator. Loader trust
    // posture mirrors the eval-side `Context` assembly below.
    let mut loader = match trust {
        TrustMode::Sandboxed => ResolverChainLoader::sandboxed(),
        TrustMode::Trusted => ResolverChainLoader::trusted(),
    };
    let entry_dir_path = PathBuf::from(&current_dir);
    let workspace = analyze_entry(cache_namespace.clone(), source, entry_dir_path, &mut loader);

    if workspace.has_errors() {
        let (ws_errs, module_errs) = workspace_error_payload(&workspace);
        return Err(Error::AnalyzeWorkspace {
            workspace: ws_errs,
            modules: module_errs,
        });
    }

    // Pull the entry's parsed root out of the workspace so we don't
    // re-parse here. `analyze_entry` already wired the entry into
    // `nodes`; on a successful workspace it's always present.
    let entry_node = workspace
        .nodes
        .get(&cache_namespace)
        .map(|arc| (**arc).clone())
        .unwrap_or_else(|| {
            // Defensive: if the workspace pass reported success but
            // didn't seed the entry node (shouldn't happen), fall
            // back to a fresh parse so the rest of the pipeline still
            // gets a Node.
            parse_document(source).expect("workspace passed but entry no longer parseable")
        });

    let workspace = Arc::new(workspace);

    // Default entry points (`value_from_str` / `value_from_file`)
    // run sandboxed: only `std/*` imports resolve, capability-gated
    // native fns are denied, no fs reads. Hosts that need the legacy
    // fully-granted runtime call the `*_trusted` variants instead.
    // Spelled out so a code reviewer sees the trust scope.
    let ctx = {
        let mut ctx = Context::sandboxed()
            .with_root(entry_node)
            .with_workspace(Arc::clone(&workspace));
        if matches!(trust, TrustMode::Trusted) {
            ctx.capabilities = Capabilities::all_granted();
            ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
        }
        Arc::new(ctx)
    };

    let _root_loading_guard = if cache_namespace == "<memory>" {
        None
    } else {
        Some(ctx.enter_loading_module(cache_namespace.clone()))
    };
    let evaluator = Evaluator::new(Arc::clone(&ctx));

    let mut root_scope = Scope::default();
    root_scope.current_dir = current_dir;
    root_scope.cache_namespace = cache_namespace;
    Ok(evaluator.eval_root(&Arc::new(root_scope))?)
}

/// Parse `source` and run the analyzer, returning the side-table tree
/// without ever touching the evaluator. Use this from a host/LSP that
/// wants static diagnostics (schema shape, untyped fields) without
/// paying for evaluation.
pub fn analyze_from_str(source: &str) -> Result<AnalyzedTree> {
    let node = parse_document(source).map_err(|err| Error::Parse(err.to_string()))?;
    Ok(analyze(&node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Deserialize, PartialEq)]
    struct ServerConfig {
        host: String,
        port: i64,
        display: String,
    }

    #[test]
    fn deserializes_from_str() {
        let config: ServerConfig = from_str(
            r#"#relaxed
        {
            #private
            format(v): "port=" + v,
            host: "localhost",
            base: { port: 8080 },
            port: &sibling.base.port,
            display: format(&sibling.port)
        }"#,
        )
        .unwrap();

        assert_eq!(
            config,
            ServerConfig {
                host: "localhost".to_string(),
                port: 8080,
                display: "port=8080".to_string(),
            }
        );
    }

    #[test]
    fn deserializes_from_file() {
        let path = std::env::temp_dir().join(format!(
            "relon-from-file-{}-{}.relon",
            std::process::id(),
            "server"
        ));
        std::fs::write(
            &path,
            r#"{
            host: "127.0.0.1",
            port: 3000,
            display: "port=3000"
        }"#,
        )
        .unwrap();

        let config: ServerConfig = from_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            config,
            ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 3000,
                display: "port=3000".to_string(),
            }
        );
    }

    #[test]
    fn custom_projector_extracts_typed_field_set() {
        // Demonstrates the Projector trait: a host can swap the default
        // JSON projection for any custom representation. Here we project
        // a Dict into a sorted `Vec<String>` of its top-level keys.
        struct KeysProjector;

        #[derive(Debug, thiserror::Error)]
        #[error("expected top-level Dict, got {0}")]
        struct NotADict(&'static str);

        impl Projector for KeysProjector {
            type Output = Vec<String>;
            type Error = NotADict;

            fn project(&self, value: &Value) -> std::result::Result<Self::Output, Self::Error> {
                match value {
                    Value::Dict(d) => {
                        let mut keys: Vec<String> = d.map.keys().cloned().collect();
                        keys.sort();
                        Ok(keys)
                    }
                    other => Err(NotADict(other.type_name())),
                }
            }
        }

        let value = value_from_str(r#"{ host: "x", port: 80, tag: "p" }"#).unwrap();
        let keys = project_with(&KeysProjector, &value).unwrap();
        assert_eq!(
            keys,
            vec!["host".to_string(), "port".to_string(), "tag".to_string()]
        );
    }

    #[test]
    fn analyzer_aggregates_multiple_schema_errors() {
        // Two independent structural problems in one source: one body that
        // isn't a dict, and one field without a type annotation. The
        // workspace analyzer should report both in a single batch
        // instead of bailing out on the first.
        let result = value_from_str(
            r#"#schema BadBody 42
#schema BadField { name: * }
{}"#,
        );

        let diags: Vec<Diagnostic> = match result {
            Err(Error::AnalyzeWorkspace { modules, .. }) => {
                modules.into_iter().map(|(_, d)| d).collect()
            }
            other => panic!("expected Error::AnalyzeWorkspace, got {other:?}"),
        };
        assert!(
            diags
                .iter()
                .any(|d| matches!(d, Diagnostic::RootSchemaInvalidValue { name, .. } if name == "BadBody")),
            "{diags:?}"
        );
        assert!(diags
            .iter()
            .any(|d| matches!(d, Diagnostic::SchemaFieldUntyped { field, .. } if field == "name")));
    }

    #[test]
    fn analyze_from_str_returns_tree_without_evaluating() {
        // `analyze_from_str` must not run the evaluator — it should
        // succeed even on programs that would crash at runtime, as long
        // as the static structure is sound. The `#relaxed` opt-out keeps
        // the head-unresolved reference at warning severity instead of
        // escalating to `UnknownReferenceType`.
        let tree = analyze_from_str(
            r#"#relaxed
#schema User { String name: * }
{ missing: &sibling.does_not_exist }"#,
        )
        .expect("analyze");
        assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
        assert_eq!(tree.schemas.len(), 1);
    }

    #[test]
    fn rejects_trailing_tokens_from_facade_api() {
        let result = value_from_str("{} true");
        assert!(matches!(result, Err(Error::Parse(message)) if message.contains("trailing input")));
    }

    #[test]
    fn json_externally_tags_sum_type_variant() {
        let value = json_from_str(
            r#"#schema Notification Enum<
    Email { address: String, subject: String },
    Push,
>
{ msg: Notification.Email { address: "a@b.c", subject: "hi" } }"#,
        )
        .unwrap();
        // The variant payload must be wrapped as `{ "Email": { ... } }`.
        let msg = value.get("msg").expect("msg key");
        let email = msg.get("Email").expect("externally-tagged Email");
        assert_eq!(email.get("address").unwrap(), "a@b.c");
        assert_eq!(email.get("subject").unwrap(), "hi");
        // The schema definition itself is dropped from JSON output.
        assert!(value.get("Notification").is_none());
    }

    #[test]
    fn json_keeps_plain_branded_dict_flat() {
        // Non-variant branded dicts (`#schema Email { ... }` standalone)
        // serialize flat — only the sum-type variants get wrapped.
        let value = json_from_str(
            r#"#schema Email { String address: * }
{ Email e: { address: "x@y.z" } }"#,
        )
        .unwrap();
        let e = value.get("e").expect("e key");
        // Flat: address sits at the top of `e`, no wrapper key.
        assert_eq!(e.get("address").unwrap(), "x@y.z");
        assert!(e.get("Email").is_none());
    }

    #[test]
    fn rejects_non_finite_floats_at_json_boundary() {
        let value = value_from_str("{ x: Infinity }").unwrap();
        assert!(matches!(value, Value::Dict(_)));

        let result = json_from_str("{ x: Infinity }");
        assert!(matches!(result, Err(Error::NonFiniteFloat(value)) if value.is_infinite()));
    }

    #[test]
    fn fixture_and_example_outputs_match_goldens() {
        let root = workspace_root();
        for file in success_relon_files(&root) {
            let rel_path = file.strip_prefix(&root).unwrap();
            // Trusted: fixtures use cross-file `#import` and the
            // sandboxed default would surface those as ModuleNotFound.
            let value = json_from_file_trusted(&file)
                .unwrap_or_else(|error| panic!("{} failed: {error}", rel_path.display()));
            let actual = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
            let expected_path = root
                .join("fixtures/golden/success")
                .join(rel_path)
                .with_extension("json");
            let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|error| {
                panic!("failed to read {}: {error}", expected_path.display())
            });

            assert_eq!(
                actual,
                expected,
                "golden mismatch for {}",
                rel_path.display()
            );
        }
    }

    #[test]
    fn main_entry_examples_match_canonical_outputs() {
        // Regression guard for the three `#main`-style examples added in
        // commit 30e2b79. The library-mode golden runner above excludes
        // them because it doesn't push args; this test drives each
        // through `Evaluator::run_main` with the canonical `--args`
        // documented in each file's header and compares JSON output
        // against a golden snapshot under `fixtures/golden/examples_main/`.
        //
        // `examples/feature_flag.relon` requires a host-registered
        // `native_hash(String) -> Int` (the example documents this in
        // its header). This test wires a deterministic stand-in so the
        // example actually runs; production hosts substitute siphash /
        // blake3 / fxhash.
        use relon_evaluator::{NativeArgs, RelonFunction};
        use relon_parser::TokenRange;

        struct StableHostHash;
        impl RelonFunction for StableHostHash {
            fn call(
                &self,
                args: NativeArgs,
                range: TokenRange,
            ) -> std::result::Result<Value, RuntimeError> {
                let positional = args.into_positional();
                if positional.len() != 1 {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "1 argument".to_string(),
                        found: positional.len().to_string(),
                        range,
                    });
                }
                let Value::String(s) = &positional[0] else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "String".to_string(),
                        found: positional[0].type_name().to_string(),
                        range,
                    });
                };
                // Deterministic, byte-stable hash. Production hosts
                // would swap in a real hash family; this one is good
                // enough that the snapshot stays stable across
                // platforms and rustc versions.
                let mut h: i64 = 0;
                for b in s.as_bytes() {
                    h = h.wrapping_mul(31).wrapping_add(*b as i64);
                }
                Ok(Value::Int(h.wrapping_abs()))
            }
        }

        type CtxSetup = fn(&mut Context);
        let no_setup: CtxSetup = |_ctx: &mut Context| {};
        let feature_flag_setup: CtxSetup = |ctx: &mut Context| {
            ctx.register_pure_fn("native_hash", Arc::new(StableHostHash));
        };

        let root = workspace_root();
        let cases: &[(&str, &str, CtxSetup)] = &[
            (
                "examples/feature_flag.relon",
                r#"{"user": {"id": "alice-42", "region": "eu", "plan": "pro"}}"#,
                feature_flag_setup,
            ),
            (
                "examples/pricing.relon",
                r#"{"order": {"tier": "gold", "items": [{"sku": "BOOK-01", "qty": 3, "unit_price": 100.0}, {"sku": "PEN-09", "qty": 4, "unit_price": 50.0}, {"sku": "DESK-22", "qty": 1, "unit_price": 300.0}]}}"#,
                no_setup,
            ),
            (
                "examples/workflow.relon",
                r#"{"state": "placed", "event": "pay"}"#,
                no_setup,
            ),
        ];

        for (rel_path, args_json, setup) in cases {
            let file = root.join(rel_path);
            let content = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("{rel_path}: failed to read: {e}"));
            let node =
                parse_document(&content).unwrap_or_else(|e| panic!("{rel_path}: parse: {e}"));
            let analyzed = Arc::new(relon_analyzer::analyze(&node));
            let args: std::collections::HashMap<String, Value> = serde_json::from_str(args_json)
                .unwrap_or_else(|e| panic!("{rel_path}: args json: {e}"));
            let mut ctx = Context::new()
                .with_root(node)
                .with_analyzed(Arc::clone(&analyzed));
            setup(&mut ctx);
            let evaluator = Evaluator::new(Arc::new(ctx));
            let value = evaluator
                .run_main(&Arc::new(Scope::default()), args)
                .unwrap_or_else(|e| panic!("{rel_path}: run_main: {e:?}"));
            let json =
                to_json_value(value).unwrap_or_else(|e| panic!("{rel_path}: to_json: {e:?}"));
            let actual = format!("{}\n", serde_json::to_string_pretty(&json).unwrap());
            let golden_path = root
                .join("fixtures/golden/examples_main")
                .join(Path::new(rel_path).file_stem().unwrap())
                .with_extension("json");
            let expected = std::fs::read_to_string(&golden_path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", golden_path.display()));
            assert_eq!(actual, expected, "golden mismatch for {rel_path}");
        }
    }

    #[test]
    fn error_fixtures_match_expected_diagnostics() {
        let root = workspace_root();
        for rel_path in [
            "fixtures/errors/circular.relon",
            "fixtures/errors/integer_overflow.relon",
            "examples/validation.relon",
        ] {
            let path = root.join(rel_path);
            let error = value_from_file_trusted(&path).expect_err("expected fixture to fail");
            let actual = format_error_golden(&root, &path, error);
            let expected_path = root
                .join("fixtures/golden/errors")
                .join(rel_path)
                .with_extension("txt");
            let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|error| {
                panic!("failed to read {}: {error}", expected_path.display())
            });

            assert_eq!(actual, expected, "diagnostic mismatch for {rel_path}");
        }
    }

    #[test]
    fn library_file_imports_into_entry() {
        // Library file is import-only: when another file `#import`s it,
        // evaluation must succeed and the imported bindings must flow
        // through. The library has no `#main(...)` and is evaluated
        // statically when used as an import.
        let dir = std::env::temp_dir().join(format!("relon-library-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.relon"), r#"{ greeting: "hi" }"#).unwrap();
        std::fs::write(
            dir.join("main.relon"),
            r#"#import lib from "./lib.relon"
            { msg: lib.greeting }"#,
        )
        .unwrap();

        let value = json_from_file_trusted(dir.join("main.relon")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(value.get("msg").and_then(|v| v.as_str()), Some("hi"));
    }

    #[test]
    fn workspace_catches_cycle_before_evaluator() {
        // Stage 0 promise: a cycle in the import graph surfaces as
        // `Error::AnalyzeWorkspace` (not `Error::Eval(...)`), meaning
        // the evaluator never starts. The fixture file imports itself,
        // which is the simplest cycle to trigger.
        let dir = std::env::temp_dir().join(format!("relon-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry_path = dir.join("self.relon");
        std::fs::write(
            &entry_path,
            r#"{
                #import self_alias from "./self.relon",
                msg: "hi"
            }"#,
        )
        .unwrap();

        // Trusted: cycle detection requires the loader to actually
        // resolve the imported file, which the sandbox loader can't.
        let result = value_from_file_trusted(&entry_path);
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Err(Error::AnalyzeWorkspace { workspace, .. }) => {
                assert!(
                    workspace
                        .iter()
                        .any(|d| matches!(d, WorkspaceDiagnostic::CircularImport { .. })),
                    "{workspace:?}"
                );
            }
            other => panic!("expected AnalyzeWorkspace(CircularImport), got {other:?}"),
        }
    }

    #[test]
    fn workspace_catches_missing_import_before_evaluator() {
        let dir = std::env::temp_dir().join(format!("relon-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry_path = dir.join("entry.relon");
        std::fs::write(
            &entry_path,
            r#"#import x from "./does_not_exist.relon"
            { v: 1 }"#,
        )
        .unwrap();
        let result = value_from_file(&entry_path);
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Err(Error::AnalyzeWorkspace { workspace, .. }) => {
                assert!(
                    workspace
                        .iter()
                        .any(|d| matches!(d, WorkspaceDiagnostic::ModuleNotFound { .. })),
                    "{workspace:?}"
                );
            }
            other => panic!("expected AnalyzeWorkspace(ModuleNotFound), got {other:?}"),
        }
    }

    #[test]
    fn unmarked_file_works_as_entry_and_as_import() {
        // The default file role is double-purpose: entry-evaluatable
        // AND importable. Sanity-check both directions on one file.
        let dir = std::env::temp_dir().join(format!("relon-unmarked-dual-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shared.relon"), r#"{ greeting: "hi" }"#).unwrap();
        std::fs::write(
            dir.join("entry.relon"),
            r#"#import s from "./shared.relon"
            { msg: s.greeting }"#,
        )
        .unwrap();

        let as_entry = json_from_file_trusted(dir.join("shared.relon")).unwrap();
        let as_imported = json_from_file_trusted(dir.join("entry.relon")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            as_entry.get("greeting").and_then(|v| v.as_str()),
            Some("hi")
        );
        assert_eq!(as_imported.get("msg").and_then(|v| v.as_str()), Some("hi"));
    }

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn success_relon_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_relon_files(&root.join("fixtures"), &mut files);
        collect_relon_files(&root.join("examples"), &mut files);
        // Exclude #main-style example entry programs: the golden runner
        // does library-mode `eval_root` without pushing host args, so a
        // file with `#main(Dict user)` would surface as `Variable not
        // found: user`. Each #main example has its own canonical args
        // documented in the file header for hands-on `cargo run` use.
        let main_entry_examples: &[&Path] = &[
            Path::new("examples/validation.relon"),
            Path::new("examples/feature_flag.relon"),
            Path::new("examples/pricing.relon"),
            Path::new("examples/workflow.relon"),
        ];
        files.retain(|file| {
            let rel_path = file.strip_prefix(root).unwrap();
            !rel_path.starts_with("fixtures/errors")
                && !rel_path.starts_with("fixtures/golden")
                && !main_entry_examples.contains(&rel_path)
        });
        files.sort();
        files
    }

    fn collect_relon_files(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                collect_relon_files(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "relon") {
                files.push(path);
            }
        }
    }

    fn format_error_golden(root: &Path, file: &Path, error: Error) -> String {
        match error {
            Error::AnalyzeWorkspace { workspace, modules } => {
                // After Stage 0, circular imports are caught by the
                // workspace analyzer rather than the evaluator. After
                // Stage 5, literal-only arithmetic faults are caught by
                // the per-module analyzer. Render whichever flavor the
                // current error carries, in the same shape the runtime
                // variant used to produce so the existing goldens still
                // apply (with the chain list refreshed to the explicit
                // start/end form).
                let source = std::fs::read_to_string(file).unwrap();
                if let Some((chain, span)) = workspace.iter().find_map(|d| match d {
                    WorkspaceDiagnostic::CircularImport { chain, range } => {
                        Some((chain.clone(), *range))
                    }
                    _ => None,
                }) {
                    let (start_line, start_column) = line_column_at(&source, span.offset());
                    let (end_line, end_column) =
                        line_column_at(&source, span.offset() + span.len());
                    let normalized_chain = chain
                        .iter()
                        .map(|path| {
                            Path::new(path)
                                .strip_prefix(root)
                                .unwrap_or_else(|_| Path::new(path))
                                .to_string_lossy()
                                .to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    return format!(
                        "CircularImport\nchain:\n{normalized_chain}\nrange: {start_line}:{start_column}-{end_line}:{end_column}\n"
                    );
                }

                // Stage 5: const-fold diagnostics surface as module-
                // level Errors. Render the first one we find so the
                // golden test remains a single deterministic block.
                if let Some(diag) = modules.iter().find_map(|(_, d)| match d {
                    Diagnostic::ConstNumericOverflow { .. }
                    | Diagnostic::ConstDivisionByZero { .. } => Some(d.clone()),
                    _ => None,
                }) {
                    return format_const_fold_diagnostic(&source, &diag);
                }

                panic!(
                    "expected CircularImport or const-fold diagnostic, got workspace={workspace:?} modules={modules:?}"
                );
            }
            Error::Eval(RuntimeError::CircularImport(chain, span)) => {
                let source = std::fs::read_to_string(file).unwrap();
                let (start_line, start_column) = line_column_at(&source, span.offset());
                let (end_line, end_column) = line_column_at(&source, span.offset() + span.len());
                let normalized_chain = chain
                    .iter()
                    .map(|path| {
                        Path::new(path)
                            .strip_prefix(root)
                            .unwrap_or_else(|_| Path::new(path))
                            .to_string_lossy()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "CircularImport\nchain:\n{normalized_chain}\nrange: {start_line}:{start_column}-{end_line}:{end_column}\n"
                )
            }
            Error::Eval(RuntimeError::TypeMismatch {
                expected,
                found,
                range,
            }) => {
                format!(
                    "TypeMismatch\nexpected: {expected}\nfound: {found}\nrange: {}:{}-{}:{}\n",
                    range.start.line, range.start.column, range.end.line, range.end.column
                )
            }
            Error::Eval(RuntimeError::NumericOverflow(range)) => {
                format!(
                    "NumericOverflow\nrange: {}:{}-{}:{}\n",
                    range.start.line, range.start.column, range.end.line, range.end.column
                )
            }
            other => panic!("unexpected error for {}: {other:?}", file.display()),
        }
    }

    /// Stage 5: render a `ConstNumericOverflow` / `ConstDivisionByZero`
    /// in the same line/column shape the runtime variant used to
    /// produce. Lets the existing `*.txt` goldens stay tiny: one tag
    /// line plus one `range:` line.
    fn format_const_fold_diagnostic(source: &str, diag: &Diagnostic) -> String {
        let (tag, span) = match diag {
            Diagnostic::ConstNumericOverflow { range, .. } => ("NumericOverflow", *range),
            Diagnostic::ConstDivisionByZero { range, .. } => ("DivisionByZero", *range),
            other => panic!("not a const-fold diagnostic: {other:?}"),
        };
        let (start_line, start_column) = line_column_at(source, span.offset());
        let (end_line, end_column) = line_column_at(source, span.offset() + span.len());
        format!("{tag}\nrange: {start_line}:{start_column}-{end_line}:{end_column}\n")
    }

    fn line_column_at(source: &str, offset: usize) -> (u32, usize) {
        let offset = offset.min(source.len());
        let mut line = 1u32;
        let mut column = 1usize;
        let mut chars = source[..offset].chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    line += 1;
                    column = 1;
                }
                '\n' => {
                    line += 1;
                    column = 1;
                }
                _ => column += 1,
            }
        }
        (line, column)
    }
}
