pub mod projector;

use relon_analyzer::{analyze, AnalyzedTree, Diagnostic};
use relon_evaluator::{Context, Evaluator, RuntimeError, Scope, Value};
use relon_parser::parse_document;
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

fn evaluate_source(
    source: &str,
    current_dir: impl Into<String>,
    cache_namespace: impl Into<String>,
) -> Result<Value> {
    let node = parse_document(source).map_err(|err| Error::Parse(err.to_string()))?;

    // Run the analyzer first so any structural problems (malformed
    // `@schema`, untyped schema fields, ...) surface as a batched
    // `Error::Analyze` before we touch the evaluator. Diagnostics at
    // `Error` severity short-circuit; warnings are silently attached to
    // the evaluator context for runtime fast-paths.
    let mut analyzed = analyze(&node);
    if analyzed.has_errors() {
        return Err(Error::Analyze(analyzed.take_diagnostics()));
    }
    let analyzed = Arc::new(analyzed);

    let cache_namespace = cache_namespace.into();
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let _root_loading_guard = if cache_namespace == "<memory>" {
        None
    } else {
        Some(ctx.enter_loading_module(cache_namespace.clone()))
    };
    let evaluator = Evaluator::new(&ctx);

    let mut root_scope = Scope::default();
    root_scope.current_dir = current_dir.into();
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
            _format(v): "port=" + v,
            host: "localhost",
            base: { port: 8080 },
            port: &sibling.base.port,
            display: _format(&sibling.port)
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
        // analyzer should report both in a single batch instead of
        // bailing out on the first.
        let result = value_from_str(
            r#"{
                @schema BadBody: 42,
                @schema BadField: { name: * }
            }"#,
        );

        let diags = match result {
            Err(Error::Analyze(diags)) => diags,
            other => panic!("expected Error::Analyze, got {other:?}"),
        };
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(diags
            .iter()
            .any(|d| matches!(d, Diagnostic::SchemaBodyNotDict { .. })));
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
            r#"{
                @schema User: { String name: * },
                missing: &sibling.does_not_exist
            }"#,
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
            r#"{
                @schema Notification: Enum<
                    Email { address: String, subject: String },
                    Push,
                >,
                msg: Notification.Email { address: "a@b.c", subject: "hi" }
            }"#,
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
        // Non-variant branded dicts (`@schema Email { ... }` standalone)
        // serialize flat — only the sum-type variants get wrapped.
        let value = json_from_str(
            r#"{
                @schema Email: { String address: * },
                Email e: { address: "x@y.z" }
            }"#,
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
