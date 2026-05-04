use relon_evaluator::{Context, Evaluator, RuntimeError, Scope, Value};
use relon_parser::parse_document;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

pub fn to_json_value(value: Value) -> Result<serde_json::Value> {
    match value {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(value) => Ok(serde_json::Value::Bool(value)),
        Value::Int(value) => Ok(serde_json::Value::Number(value.into())),
        Value::Float(value) => {
            let value = value.into_inner();
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .ok_or(Error::NonFiniteFloat(value))
        }
        Value::String(value) => Ok(serde_json::Value::String(value)),
        Value::List(values) => values
            .into_iter()
            .map(to_json_value)
            .collect::<Result<Vec<_>>>()
            .map(serde_json::Value::Array),
        Value::Dict(values) => {
            let mut map = serde_json::Map::new();
            for (key, value) in values.map {
                match value {
                    Value::Closure { .. } => continue, // Skip closures in dicts
                    Value::Schema(_) => continue,      // Skip schemas in dicts
                    Value::Type(_) => continue,        // Skip types in dicts
                    Value::Wildcard => continue,       // Skip wildcards in dicts
                    _ => {
                        map.insert(key, to_json_value(value)?);
                    }
                }
            }
            Ok(serde_json::Value::Object(map))
        }
        Value::Closure { .. } => Err(Error::UnsupportedClosure),
        Value::Schema(_) => Err(Error::UnsupportedSchema),
        Value::Type(_) => Err(Error::UnsupportedSchema),
        Value::Wildcard => Err(Error::UnsupportedSchema),
    }
}

fn evaluate_source(
    source: &str,
    current_dir: impl Into<String>,
    cache_namespace: impl Into<String>,
) -> Result<Value> {
    let node = parse_document(source).map_err(|err| Error::Parse(err.to_string()))?;
    let cache_namespace = cache_namespace.into();
    let ctx = Context::new().with_root(node.clone());
    let _root_loading_guard = if cache_namespace == "<memory>" {
        None
    } else {
        Some(ctx.enter_loading_module(cache_namespace.clone()))
    };
    let evaluator = Evaluator::new(&ctx);

    let mut root_scope = Scope::default();
    root_scope.current_dir = current_dir.into();
    root_scope.cache_namespace = cache_namespace;
    root_scope.reference_root = Some(Arc::new(node.clone()));
    Ok(evaluator.eval(&node, &Arc::new(root_scope))?)
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
    fn rejects_trailing_tokens_from_facade_api() {
        let result = value_from_str("{} true");
        assert!(matches!(result, Err(Error::Parse(message)) if message.contains("trailing input")));
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
