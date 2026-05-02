use relon_evaluator::{Context, Evaluator, RuntimeError, Scope, Value};
use relon_parser::{parse_base, Span};
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
    Ok(to_json_value(value_from_str(source)?))
}

pub fn json_from_file(path: impl AsRef<Path>) -> Result<serde_json::Value> {
    Ok(to_json_value(value_from_file(path)?))
}

pub fn value_from_str(source: &str) -> Result<Value> {
    evaluate_source(source, ".", "<memory>")
}

pub fn value_from_file(path: impl AsRef<Path>) -> Result<Value> {
    let path = path.as_ref();
    let source = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let current_dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .to_string_lossy()
        .to_string();
    evaluate_source(&source, current_dir, path.to_string_lossy().to_string())
}

pub fn to_json_value(value: Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(value),
        Value::Int(value) => serde_json::Value::Number(value.into()),
        Value::Float(value) => serde_json::Number::from_f64(value.into_inner())
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(value) => serde_json::Value::String(value),
        Value::List(values) => {
            serde_json::Value::Array(values.into_iter().map(to_json_value).collect())
        }
        Value::Dict(values) => {
            let mut map = serde_json::Map::new();
            for (key, value) in values {
                if !key.starts_with('_') {
                    map.insert(key, to_json_value(value));
                }
            }
            serde_json::Value::Object(map)
        }
        Value::Closure { .. } => serde_json::Value::String("<closure>".to_string()),
    }
}

fn evaluate_source(
    source: &str,
    current_dir: impl Into<String>,
    cache_namespace: impl Into<String>,
) -> Result<Value> {
    let mut input = Span::new(source);
    let node = parse_base(&mut input).map_err(|err| Error::Parse(format!("{err:?}")))?;
    let ctx = Context::new().with_root(node.clone());
    let evaluator = Evaluator::new(&ctx);

    let mut root_scope = Scope::default();
    root_scope.current_dir = current_dir.into();
    root_scope.cache_namespace = cache_namespace.into();
    Ok(evaluator.eval(&node, &Arc::new(root_scope))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

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
            _format: @fn(v) "port=" + v,
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
}
