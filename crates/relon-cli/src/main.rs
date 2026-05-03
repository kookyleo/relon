use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, LabeledSpan, NamedSource, Report};
use relon_evaluator::{Context, Evaluator, Scope, Value};
use relon_parser::{parse_document, ParseDocumentError};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "relon")]
#[command(about = "Relon: A programmable configuration language", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Evaluate a .relon file and output JSON
    Run {
        /// The path to the .relon file
        file: PathBuf,
        /// Pretty-print the output JSON
        #[arg(short, long, default_value_t = true)]
        pretty: bool,
    },
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { file, pretty } => {
            let canonical_file = std::fs::canonicalize(&file)
                .into_diagnostic()
                .map_err(|e| e.wrap_err(format!("Failed to resolve file {:?}", file)))?;
            let content = std::fs::read_to_string(&canonical_file)
                .into_diagnostic()
                .map_err(|e| e.wrap_err(format!("Failed to read file {:?}", canonical_file)))?;

            let node = parse_document(&content).map_err(|e| {
                let report = if let Some(span) = e.source_span() {
                    let label = match &e {
                        ParseDocumentError::Parse { .. } => "parse error",
                        ParseDocumentError::TrailingInput { .. } => "invalid trailing input",
                    };
                    miette::miette!(
                        labels = vec![LabeledSpan::new_with_span(Some(label.to_string()), span)],
                        "Parse error: {}",
                        e
                    )
                } else {
                    miette::miette!("Parse error: {}", e)
                };
                report.with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
            })?;

            let ctx = Context::new().with_root(node.clone());
            let cache_namespace = canonical_file.to_string_lossy().to_string();
            let _root_loading_guard = ctx.enter_loading_module(cache_namespace.clone());
            let evaluator = Evaluator::new(&ctx);

            let mut root_scope = Scope::default();
            root_scope.current_dir = canonical_file
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_string_lossy()
                .to_string();
            root_scope.cache_namespace = cache_namespace;
            root_scope.reference_root = Some(std::sync::Arc::new(node.clone()));
            let scope = std::sync::Arc::new(root_scope);

            let result = evaluator.eval(&node, &scope).map_err(|e| {
                Report::new(e)
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
            })?;

            // Filter out private fields (starting with _) before JSON output
            let final_val = filter_private_fields(result)?;

            let output = if pretty {
                serde_json::to_string_pretty(&final_val).into_diagnostic()?
            } else {
                serde_json::to_string(&final_val).into_diagnostic()?
            };

            println!("{}", output);
        }
    }

    Ok(())
}

fn filter_private_fields(val: Value) -> miette::Result<serde_json::Value> {
    match val {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(b)),
        Value::Int(i) => Ok(serde_json::Value::Number(i.into())),
        Value::Float(f) => {
            let value = f.into_inner();
            serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| {
                    miette::miette!("non-finite float {value} cannot be represented in JSON")
                })
        }
        Value::String(s) => Ok(serde_json::Value::String(s)),
        Value::List(l) => l
            .into_iter()
            .map(filter_private_fields)
            .collect::<miette::Result<Vec<_>>>()
            .map(serde_json::Value::Array),
        Value::Dict(m) => {
            let mut map = serde_json::Map::new();
            for (k, v) in m {
                if !k.starts_with('_') {
                    map.insert(k, filter_private_fields(v)?);
                }
            }
            Ok(serde_json::Value::Object(map))
        }
        Value::Closure { .. } => Ok(serde_json::Value::String("<closure>".to_string())),
    }
}
