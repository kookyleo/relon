use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, LabeledSpan, NamedSource, Report};
use relon_evaluator::module::FilesystemModuleResolver;
use relon_evaluator::{Capabilities, Context, Evaluator, Scope};
use relon_parser::{parse_document, ParseDocumentError};
use std::path::PathBuf;
use std::sync::Arc;

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

            // The CLI is the host's own trusted entry point: it runs
            // files the operator explicitly hands it, so it grants
            // every capability. The grant is written out so a code
            // reviewer can see what's being trusted.
            //
            // We also attach the analyzer side-table so root-level
            // contracts (`#main(...)` signature, root `#schema X Body`
            // declarations, schema desugar fast-paths) reach the
            // evaluator the same way they do through the high-level
            // `relon::*` facade.
            let analyzed = relon_analyzer::analyze(&node);
            if analyzed.has_errors() {
                let diags = analyzed
                    .diagnostics
                    .iter()
                    .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join("\n  - ");
                return Err(miette::miette!("Analyzer reported errors:\n  - {diags}")
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
            }
            let analyzed = Arc::new(analyzed);
            let ctx = {
                let mut ctx = Context::sandboxed()
                    .with_root(node)
                    .with_analyzed(Arc::clone(&analyzed));
                ctx.capabilities = Capabilities::all_granted();
                ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
                Arc::new(ctx)
            };
            let cache_namespace = canonical_file.to_string_lossy().to_string();
            let _root_loading_guard = ctx.enter_loading_module(cache_namespace.clone());
            let evaluator = Evaluator::new(Arc::clone(&ctx));

            let mut root_scope = Scope::default();
            root_scope.current_dir = canonical_file
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_string_lossy()
                .to_string();
            root_scope.cache_namespace = cache_namespace;
            let scope = std::sync::Arc::new(root_scope);

            let result = evaluator.eval_root(&scope).map_err(|e| {
                Report::new(e)
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
            })?;

            let final_val = relon::to_json_value(result).map_err(|e| {
                miette::miette!("{}", e)
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content))
            })?;

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
