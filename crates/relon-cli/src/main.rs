#![forbid(unsafe_code)]

use clap::{Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic, LabeledSpan, NamedSource, Report};
use relon::ResolverChainLoader;
use relon_analyzer::analyze_entry;
use relon_codegen_wasm::WasmAotEvaluator;
use relon_eval_api::Evaluator as EvaluatorTrait;
use relon_evaluator::module::FilesystemModuleResolver;
use relon_evaluator::{Capabilities, Context, Scope, TreeWalkEvaluator, Value};
use relon_parser::{parse_document, ParseDocumentError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Mirror of [`relon::Backend`] surfaced as a clap-friendly enum so the
/// CLI accepts `--backend=tree-walk` / `--backend=wasm-aot`. Kept
/// separate from the public type so the rename / display knobs stay
/// CLI-only — the library facade picks ergonomic Rust names while the
/// CLI sticks with kebab-case for muscle-memory.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum BackendArg {
    /// Tree-walking interpreter (default). Supports the full
    /// `Evaluator` surface including arbitrary `eval` on AST nodes
    /// and host-registered native fns.
    #[default]
    #[value(name = "tree-walk")]
    TreeWalk,
    /// Wasm AOT backend. Pre-compiles the entry into wasm and
    /// dispatches `run_main` through wasmtime. Library-mode
    /// `eval_root` is rejected — wasm-AOT only ships entries.
    #[value(name = "wasm-aot")]
    WasmAot,
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
        /// registered with a non-empty `NativeFnGate` are denied
        /// (pure fns registered via `register_pure_fn` keep working).
        /// Pass `--trust` when the script expects the legacy
        /// fully-granted environment (e.g. local imports, registered
        /// HTTP / FS helpers).
        #[arg(long)]
        trust: bool,
        /// Evaluation backend. `tree-walk` (default) runs the original
        /// interpreter; `wasm-aot` pre-compiles the entry into a
        /// wasm module via `relon-codegen-wasm` and dispatches
        /// `run_main` through wasmtime. The wasm-aot backend only
        /// supports `#main(...)` entries and rejects library-mode
        /// (no-#main) sources.
        #[arg(long, value_enum, default_value_t = BackendArg::TreeWalk)]
        backend: BackendArg,
    },

    /// Format or check Relon files. Equivalent to the standalone
    /// `relon-fmt` binary, exposed here so a single `relon` install
    /// covers the toolchain.
    Fmt {
        /// Check whether files are formatted without writing changes.
        #[arg(long)]
        check: bool,
        /// Print formatted output to stdout instead of writing files.
        #[arg(long)]
        stdout: bool,
        /// Relon files to format.
        files: Vec<PathBuf>,
    },

    /// Run the Language Server (stdio transport). Equivalent to the
    /// standalone `relon-lsp` binary; editors that want a single
    /// `relon lsp` command can wire to this.
    Lsp,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            file,
            pretty,
            args,
            trust,
            backend,
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
                ResolverChainLoader::trusted()
            } else {
                ResolverChainLoader::sandboxed()
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
                Arc::new({
                    let mut ctx = ctx;
                    relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
                    ctx
                })
            };
            let _root_loading_guard = ctx.enter_loading_module(cache_namespace.clone());
            let evaluator = TreeWalkEvaluator::new(Arc::clone(&ctx));

            let scope = std::sync::Arc::new(Scope {
                current_dir: canonical_file
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_string_lossy()
                    .to_string(),
                cache_namespace: cache_namespace.clone(),
                ..Scope::default()
            });

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
                match backend {
                    BackendArg::TreeWalk => evaluator.run_main(&scope, args_map).map_err(|e| {
                        Report::new(e)
                            .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
                    })?,
                    BackendArg::WasmAot => {
                        // wasm-AOT compiles the single entry file
                        // (no transitive imports in v1) through
                        // codegen + wasmtime and dispatches the same
                        // host-args HashMap through `run_main`. We
                        // intentionally re-parse / re-analyze inside
                        // `from_source` rather than reusing the
                        // workspace tree: the codegen path wants the
                        // single-file analyzer output, and folding
                        // workspace mode into v1 is a Phase 9 goal.
                        let aot = WasmAotEvaluator::from_source(&content).map_err(|e| {
                            miette::miette!("wasm-aot backend setup: {e}").with_source_code(
                                NamedSource::new(file.to_string_lossy(), content.clone()),
                            )
                        })?;
                        EvaluatorTrait::run_main(&aot, args_map).map_err(|e| {
                            Report::new(e).with_source_code(NamedSource::new(
                                file.to_string_lossy(),
                                content.clone(),
                            ))
                        })?
                    }
                }
            } else {
                if args.is_some() {
                    return Err(miette::miette!(
                        "--args was provided but the file has no `#main(...)` signature"
                    )
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
                }
                if matches!(backend, BackendArg::WasmAot) {
                    return Err(miette::miette!(
                        "wasm-aot backend only supports `#main(...)` entries; the file declares no signature"
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
        Commands::Fmt {
            check,
            stdout,
            files,
        } => {
            run_fmt(check, stdout, files)?;
        }
        Commands::Lsp => {
            relon_lsp::server::run_stdio()
                .map_err(|e| miette::miette!("language server exited with error: {e}"))?;
        }
    }

    Ok(())
}

/// Same logic as the standalone `relon-fmt` binary. Kept here so a
/// single `relon` install covers fmt without an extra crate
/// dependency on the operator's side; the standalone binary stays
/// available for scripts that already wire to it.
fn run_fmt(check: bool, stdout: bool, files: Vec<PathBuf>) -> miette::Result<()> {
    if files.is_empty() {
        return Err(miette::miette!("expected at least one file"));
    }

    let mut failed_check = false;
    for file in &files {
        let source = std::fs::read_to_string(file)
            .into_diagnostic()
            .map_err(|e| e.wrap_err(format!("failed to read {}", file.display())))?;
        let formatted = relon_fmt::format_source(&source)
            .map_err(|e| miette::miette!("{}: {e}", file.display()))?;

        if check {
            if formatted != source {
                eprintln!("{} is not formatted", file.display());
                failed_check = true;
            }
            continue;
        }

        if stdout {
            print!("{formatted}");
        } else if formatted != source {
            std::fs::write(file, formatted)
                .into_diagnostic()
                .map_err(|e| e.wrap_err(format!("failed to write {}", file.display())))?;
        }
    }

    if failed_check {
        Err(miette::miette!("one or more files were not formatted"))
    } else {
        Ok(())
    }
}
