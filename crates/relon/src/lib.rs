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

pub fn from_str<T>(source: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = json_from_str(source)?;
    Ok(serde_json::from_value(value)?)
}

pub fn from_file<T>(path: impl AsRef<Path>) -> Result<T>
where
    T: DeserializeOwned,
{
    let value = json_from_file(path)?;
    Ok(serde_json::from_value(value)?)
}

pub fn json_from_str(source: &str) -> Result<serde_json::Value> {
    to_json_value(value_from_str(source)?)
}

pub fn json_from_file(path: impl AsRef<Path>) -> Result<serde_json::Value> {
    to_json_value(value_from_file(path)?)
}

pub fn value_from_str(source: &str) -> Result<Value> {
    evaluate_source(source, ".", "<memory>")
}

pub fn value_from_file(path: impl AsRef<Path>) -> Result<Value> {
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
    )
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
struct FacadeLoader {
    resolvers: Vec<Arc<dyn ModuleResolver>>,
}

impl FacadeLoader {
    fn new_trusted() -> Self {
        // Mirror the resolver chain that `evaluate_source` mounts on
        // the `Context` below: `std/*` first, then trusted filesystem
        // fallback. Any host change to one chain has to be made to the
        // other; centralized here so future refactors can collapse the
        // two assemblies.
        Self {
            resolvers: vec![
                Arc::new(StdModuleResolver),
                Arc::new(FilesystemModuleResolver::trusted()),
            ],
        }
    }
}

impl ModuleLoader for FacadeLoader {
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
    // surface here — before we touch the evaluator.
    let mut loader = FacadeLoader::new_trusted();
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

    // `value_from_str` / `value_from_file` are the host's "evaluate
    // this Relon document and give me the JSON" entry points — by
    // construction the file is host-owned, so every capability is
    // granted. Spelled out so a code reviewer sees the trust scope.
    let ctx = {
        let mut ctx = Context::sandboxed()
            .with_root(entry_node)
            .with_workspace(Arc::clone(&workspace));
        ctx.capabilities = Capabilities::all_granted();
        ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
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
            r#"{
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
        // as the static structure is sound.
        let tree = analyze_from_str(
            r#"#schema User { String name: * }
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
            let value = json_from_file(&file)
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
    fn error_fixtures_match_expected_diagnostics() {
        let root = workspace_root();
        for rel_path in [
            "fixtures/errors/circular.relon",
            "fixtures/errors/integer_overflow.relon",
            "examples/validation.relon",
        ] {
            let path = root.join(rel_path);
            let error = value_from_file(&path).expect_err("expected fixture to fail");
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

        let value = json_from_file(dir.join("main.relon")).unwrap();
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

        let result = value_from_file(&entry_path);
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

        let as_entry = json_from_file(dir.join("shared.relon")).unwrap();
        let as_imported = json_from_file(dir.join("entry.relon")).unwrap();
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
        files.retain(|file| {
            let rel_path = file.strip_prefix(root).unwrap();
            !rel_path.starts_with("fixtures/errors")
                && !rel_path.starts_with("fixtures/golden")
                && rel_path != Path::new("examples/validation.relon")
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
            Error::AnalyzeWorkspace { workspace, .. } => {
                // After Stage 0, circular imports are caught by the
                // workspace analyzer rather than the evaluator. Render
                // the first CircularImport diagnostic in the same
                // shape the runtime variant used to produce so the
                // existing goldens still apply (with the chain list
                // refreshed to the explicit start/end form).
                let source = std::fs::read_to_string(file).unwrap();
                let circular = workspace
                    .iter()
                    .find_map(|d| match d {
                        WorkspaceDiagnostic::CircularImport { chain, range } => {
                            Some((chain.clone(), *range))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("expected CircularImport, got {workspace:?}"));
                let (chain, span) = circular;
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
