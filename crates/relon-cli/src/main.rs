use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, NamedSource, Report};
use relon_evaluator::{Context, Evaluator, Scope, Value};
use relon_parser::{parse_base, Span};
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
            let content = std::fs::read_to_string(&file)
                .into_diagnostic()
                .map_err(|e| e.wrap_err(format!("Failed to read file {:?}", file)))?;

            let mut input = Span::new(&content);
            let node =
                parse_base(&mut input).map_err(|e| miette::miette!("Parse error: {:?}", e))?;

            let ctx = Context::new().with_root(node.clone());
            let evaluator = Evaluator::new(&ctx);

            let mut root_scope = Scope::default();
            root_scope.current_dir = file
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_string_lossy()
                .to_string();
            root_scope.cache_namespace = file.to_string_lossy().to_string();
            let scope = std::sync::Arc::new(root_scope);

            let result = evaluator.eval(&node, &scope).map_err(|e| {
                Report::new(e)
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
            })?;

            // Filter out private fields (starting with _) before JSON output
            let final_val = filter_private_fields(result);

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

fn filter_private_fields(val: Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(b),
        Value::Int(i) => serde_json::Value::Number(i.into()),
        Value::Float(f) => {
            serde_json::Value::Number(serde_json::Number::from_f64(f.into_inner()).unwrap())
        }
        Value::String(s) => serde_json::Value::String(s),
        Value::List(l) => {
            serde_json::Value::Array(l.into_iter().map(filter_private_fields).collect())
        }
        Value::Dict(m) => {
            let mut map = serde_json::Map::new();
            for (k, v) in m {
                if !k.starts_with('_') {
                    map.insert(k, filter_private_fields(v));
                }
            }
            serde_json::Value::Object(map)
        }
        Value::Closure { .. } => serde_json::Value::String("<closure>".to_string()),
    }
}
