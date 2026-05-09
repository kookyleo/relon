use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, LabeledSpan, NamedSource, Report};
use relon_analyzer::{
    analyze_entry, LoadError, LoadedModule, ModuleLoader as AnalyzerModuleLoader,
};
use relon_evaluator::module::{FilesystemModuleResolver, ModuleResolver, StdModuleResolver};
use relon_evaluator::{Capabilities, Context, Evaluator, RuntimeError, Scope, Value};
use relon_parser::{parse_document, ParseDocumentError, TokenRange};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Same shape as `relon::FacadeLoader` — kept inline in the CLI so the
/// binary doesn't have to depend on the facade's `pub(crate)` types.
struct CliLoader {
    resolvers: Vec<Arc<dyn ModuleResolver>>,
}

impl CliLoader {
    fn new_sandboxed() -> Self {
        Self {
            resolvers: vec![Arc::new(StdModuleResolver)],
        }
    }

    fn new_trusted() -> Self {
        Self {
            resolvers: vec![
                Arc::new(StdModuleResolver),
                Arc::new(FilesystemModuleResolver::trusted()),
            ],
        }
    }
}

impl AnalyzerModuleLoader for CliLoader {
    fn load(&mut self, path: &str, current_dir: &Path) -> Result<LoadedModule, LoadError> {
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
                Err(other) => return Err(LoadError::Other(other.to_string())),
            }
        }
        Err(LoadError::NotFound)
    }
}

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
        /// JSON object whose keys are `#main(...)` parameter names and
        /// whose values are the host-pushed args. Required if the
        /// target file declares a `#main(...)` signature; ignored
        /// otherwise. Either pass JSON inline (`--args '{"u": ...}'`)
        /// or via stdin redirect (`--args -` reads from stdin).
        #[arg(long)]
        args: Option<String>,
        /// Grant full filesystem and capability-gated native-fn
        /// access. By default the runtime runs sandboxed: only
        /// `std/*` imports resolve, local `#import "./foo.relon"`
        /// paths surface as `ModuleNotFound`, and native fns
        /// registered via `register_fn_with_caps` are denied. Pass
        /// `--trust` when the script expects the legacy fully-granted
        /// environment (e.g. local imports, registered HTTP / FS
        /// helpers).
        #[arg(long)]
        trust: bool,
    },
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            file,
            pretty,
            args,
            trust,
        } => {
            let canonical_file = std::fs::canonicalize(&file)
                .into_diagnostic()
                .map_err(|e| e.wrap_err(format!("Failed to resolve file {:?}", file)))?;
            let content = std::fs::read_to_string(&canonical_file)
                .into_diagnostic()
                .map_err(|e| e.wrap_err(format!("Failed to read file {:?}", canonical_file)))?;

            // The CLI runs sandboxed by default: the operator passes
            // `--trust` when the script needs filesystem `#import`
            // paths or capability-gated native fns. The grant is
            // written out so a code reviewer can see what's being
            // trusted.
            //
            // Run the workspace analyzer over the entry + every
            // transitive `#import` so cycles, missing modules,
            // collisions, and per-file structural errors all surface
            // here — before the evaluator gets a chance to start.
            let cache_namespace = canonical_file.to_string_lossy().to_string();
            let entry_dir = canonical_file
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf();
            let mut loader = if trust {
                CliLoader::new_trusted()
            } else {
                CliLoader::new_sandboxed()
            };
            let workspace =
                analyze_entry(cache_namespace.clone(), &content, entry_dir, &mut loader);
            if workspace.has_errors() {
                // Surface entry parse errors with the same labelled
                // span treatment the CLI used to give pre-workspace,
                // so the operator still sees the offending position
                // rather than just a generic "Parse error".
                if let Some(parse_msg) =
                    workspace
                        .workspace_diagnostics
                        .iter()
                        .find_map(|d| match d {
                            relon_analyzer::WorkspaceDiagnostic::ModuleParseError {
                                path,
                                message,
                                ..
                            } if path == &cache_namespace => Some(message.clone()),
                            _ => None,
                        })
                {
                    if let Err(e) = parse_document(&content) {
                        let report = if let Some(span) = e.source_span() {
                            let label = match &e {
                                ParseDocumentError::Parse { .. } => "parse error",
                                ParseDocumentError::TrailingInput { .. } => {
                                    "invalid trailing input"
                                }
                            };
                            miette::miette!(
                                labels =
                                    vec![LabeledSpan::new_with_span(Some(label.to_string()), span)],
                                "Parse error: {}",
                                e
                            )
                        } else {
                            miette::miette!("Parse error: {}", e)
                        };
                        return Err(report
                            .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
                    }
                    // Defensive: workspace flagged a parse error but
                    // re-parsing succeeded — fall through to the
                    // generic message so we surface *something*.
                    return Err(miette::miette!("Parse error in entry module: {parse_msg}")
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
                }

                let mut messages: Vec<String> = workspace
                    .workspace_diagnostics
                    .iter()
                    .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                    .map(|d| d.to_string())
                    .collect();
                for (id, tree) in &workspace.modules {
                    for d in &tree.diagnostics {
                        if d.severity() == relon_analyzer::Severity::Error {
                            messages.push(format!("[{id}] {d}"));
                        }
                    }
                }
                let joined = messages.join("\n  - ");
                return Err(miette::miette!("Analyzer reported errors:\n  - {joined}")
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
            }

            // Pull the entry's parsed root out of the workspace so
            // we don't double-parse. `analyze_entry` populated
            // `nodes` for every successfully-resolved module, and on
            // a clean workspace the entry is always present. Using
            // this node — instead of a separate `parse_document` —
            // keeps `with_root` and `with_workspace` aligned on the
            // same `NodeId`s; the analyzer's resolved-reference
            // table indexes by id, so they have to match.
            let entry_node = workspace
                .nodes
                .get(&cache_namespace)
                .map(|arc| (**arc).clone())
                .expect("workspace passed has_errors check but entry node missing");
            let workspace = Arc::new(workspace);
            let ctx = {
                let mut ctx = Context::sandboxed()
                    .with_root(entry_node)
                    .with_workspace(Arc::clone(&workspace));
                if trust {
                    ctx.capabilities = Capabilities::all_granted();
                    ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
                }
                Arc::new(ctx)
            };
            let _root_loading_guard = ctx.enter_loading_module(cache_namespace.clone());
            let evaluator = Evaluator::new(Arc::clone(&ctx));

            let mut root_scope = Scope::default();
            root_scope.current_dir = canonical_file
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_string_lossy()
                .to_string();
            root_scope.cache_namespace = cache_namespace.clone();
            let scope = std::sync::Arc::new(root_scope);

            // Branch on whether the file declares `#main(...)`. With a
            // signature, hosts push args; without one, we walk the body
            // as plain data. Both shapes share the same root scope.
            let has_main = workspace
                .modules
                .get(&cache_namespace)
                .and_then(|tree| tree.main_signature.as_ref())
                .is_some();
            let result = if has_main {
                let args_json = match args.as_deref() {
                    Some("-") => {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin()
                            .read_to_string(&mut buf)
                            .into_diagnostic()
                            .map_err(|e| e.wrap_err("Failed to read --args from stdin"))?;
                        buf
                    }
                    Some(other) => other.to_string(),
                    None => {
                        return Err(miette::miette!(
                            "File declares `#main(...)`; pass --args '<json>' (or --args -) to provide host arguments"
                        )
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
                    }
                };
                let args_map: HashMap<String, Value> =
                    serde_json::from_str(&args_json).map_err(|e| {
                        miette::miette!(
                            "--args must be a JSON object keyed by `#main(...)` parameter names: {e}"
                        )
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
                    })?;
                evaluator.run_main(&scope, args_map).map_err(|e| {
                    Report::new(e)
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
                })?
            } else {
                if args.is_some() {
                    return Err(miette::miette!(
                        "--args was provided but the file has no `#main(...)` signature"
                    )
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
                }
                evaluator.eval_root(&scope).map_err(|e| {
                    Report::new(e)
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
                })?
            };

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
