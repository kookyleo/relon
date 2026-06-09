#![forbid(unsafe_code)]

use clap::{Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic, LabeledSpan, NamedSource, Report};
// Public surface lives on the `relon` facade: `Evaluator` (the
// backend-agnostic trait), `Scope` / `Value` (the canonical runtime
// data shapes), plus the `ResolverChainLoader` already routed here.
// Runtime-impl types (`Context`, `TreeWalkEvaluator`,
// `FilesystemModuleResolver`, `Capabilities`) remain direct reach
// into `relon-evaluator` — the CLI's cold-start fast paths (`--lite`,
// trivial-`#main` auto-detect, cache probing) need the lower-level
// surface the facade deliberately doesn't expose, and the CLI
// already declares a direct dep on `relon-evaluator` for them.
use relon::{Evaluator as EvaluatorTrait, ResolverChainLoader, Scope, Value};
use relon_analyzer::{
    analyze_entry_with_options, analyze_with_options, format_type, substitute_generics_in_typenode,
    AnalyzeOptions, AnalyzedTree, MainSignature, SchemaDef, WorkspaceDiagnostic, WorkspaceTree,
};
use relon_codegen_cranelift::AotEvaluator;
use relon_evaluator::module::FilesystemModuleResolver;
use relon_evaluator::{Capabilities, Context, TreeWalkEvaluator};
use relon_parser::{parse_document, ParseDocumentError, TypeNode};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Mirror of [`relon::Backend`] surfaced as a clap-friendly enum so the
/// CLI accepts `--backend=auto` / `--backend=tree-walk` /
/// `--backend=cranelift-aot`. Kept separate from the public type so the
/// rename / display knobs stay CLI-only — the library facade picks
/// ergonomic Rust names while the CLI sticks with kebab-case for
/// muscle-memory.
#[derive(Debug, Clone, Copy, Default, ValueEnum, PartialEq, Eq)]
enum BackendArg {
    /// Auto-tier (default). Routes `run_main` through cranelift-AOT
    /// on demand (lazily constructed on first call) and every other
    /// `Evaluator` method through the tree-walker. Inherits the
    /// tree-walker's full surface when the script never asks for
    /// `run_main`; pays the AOT cold-start exactly when needed.
    #[default]
    #[value(name = "auto")]
    Auto,
    /// Tree-walking interpreter only. Supports the full
    /// `Evaluator` surface including arbitrary `eval` on AST nodes
    /// and host-registered native fns.
    #[value(name = "tree-walk")]
    TreeWalk,
    /// Cranelift-native AOT backend. Lowers the entry into native
    /// machine code via cranelift's JIT and dispatches `run_main`
    /// through a panic-shielded trampoline. Library-mode `eval_root`
    /// is rejected — this backend only ships entries.
    #[value(name = "cranelift-aot")]
    CraneliftAot,
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
        /// v6-fix-D2 cold-start lite mode. Short-circuits every
        /// startup-side lazy init the default `relon run` path
        /// would normally pay: forces `Backend::TreeWalk` (no
        /// cranelift-AOT lower / JIT), skips the trust-only
        /// remote-HTTP resolver mount, skips the optional warm-up
        /// tick, and avoids constructing or probing the on-disk
        /// AOT cache directory.
        ///
        /// Differs from `--backend tree-walk`: that flag only
        /// switches the evaluator selection but still walks the
        /// same default-mode startup sequence; `--lite` *also*
        /// gates every other lazy init that doesn't pay for a
        /// short-lived `relon run foo.relon` invocation. Pairs
        /// best with the W11 bench shape (single-shot evaluator,
        /// no JIT tier worth amortising).
        #[arg(long, default_value_t = false)]
        lite: bool,
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
        /// Evaluation backend. `auto` (default) routes `run_main`
        /// through cranelift-AOT lazily (built on first invocation)
        /// and keeps every other operation on the tree-walker.
        /// `tree-walk` forces the original interpreter on every
        /// path; `cranelift-aot` pre-compiles the entry into native
        /// machine code via `relon-codegen-cranelift`'s cranelift JIT
        /// and dispatches `run_main` through a panic-shielded
        /// trampoline. The cranelift-aot-only backend supports
        /// `#main(...)` entries only and rejects library-mode
        /// (no-#main) sources; `auto` falls back to the tree-walker
        /// for library-mode files.
        #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
        backend: BackendArg,
        /// v3++ b-2: when set, every `#import` whose path looks remote
        /// (`https://`, `http://`) MUST carry an inline integrity pin
        /// (e.g. `sha256:"<hex>"`). Missing pins surface as
        /// `ImportHashRequired` *before* the loader fetches anything,
        /// so a CI gate can refuse unpinned dependencies without
        /// touching the network. Local-path imports are exempt — the
        /// supply-chain risk targeted here is wire-time, not source-
        /// tree-time. Default off preserves the v3+ a-3 behavior.
        #[arg(long, default_value_t = false)]
        require_hash: bool,
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
    ///
    /// Phase G.W11 Phase 2 cold-start: the subcommand only ships
    /// when the `cli-lsp` cargo feature is enabled. The standalone
    /// `relon-lsp` binary stays the canonical LSP entry; embedding
    /// it here cost ~160 KB text on every CLI invocation, which the
    /// W11 trivial-`#main` shape pays without ever entering. Editors
    /// that want one-binary ergonomics rebuild with
    /// `cargo build --features cli-lsp` (or `--features full`).
    #[cfg(feature = "cli-lsp")]
    Lsp,
}

/// v6-fix-D2-J cold-start: hand-rolled argv fast-path for the
/// trivial `relon run <file> [--lite] [--args <json>]` invocation
/// shape the W11 bench exercises. Constructing `Cli` through
/// `clap::Cli::parse()` is ~270 µs on a fresh process — clap rebuilds
/// the whole command tree (subcommands + value enums + help strings)
/// before matching a 3 - 5 element argv. The match here is conservative:
/// any unrecognised arg, prefix abbreviation, value-form (`--args=foo`),
/// short flag, or extra positional falls back to `clap::Cli::parse()`
/// so flag semantics stay defined by the `#[derive(Parser)]` block
/// (single source of truth). The shape we accept by design:
///
/// * `relon run <file>` (positional only)
/// * `relon run <file> --args <json>` (or `--args -` for stdin)
/// * `relon run <file> --lite [--args <json>]`
///
/// Anything else — `fmt`, `lsp`, `--trust`, `--backend`,
/// `--require-hash`, `--pretty`, `--help`, `--version` — defers to
/// clap so the surface stays compatible.
fn try_parse_run_fast(argv: &[std::ffi::OsString]) -> Option<Commands> {
    // argv[0] is the binary. The first command-line arg must be `run`;
    // everything else (`fmt`, `lsp`, `--help`, …) goes through clap.
    let mut iter = argv.iter().skip(1);
    let sub = iter.next()?;
    if sub != "run" {
        return None;
    }

    let mut file: Option<PathBuf> = None;
    let mut lite = false;
    let mut args: Option<String> = None;

    while let Some(tok) = iter.next() {
        let s = tok.to_str()?;
        if s == "--lite" {
            if lite {
                return None;
            }
            lite = true;
        } else if s == "--args" {
            if args.is_some() {
                return None;
            }
            let v = iter.next()?.to_str()?.to_string();
            args = Some(v);
        } else if s.starts_with("--") || s.starts_with('-') {
            // Any other flag (`--args=...`, `--trust`, `--pretty`,
            // `--help`, short flags) bails so clap owns the parse.
            return None;
        } else {
            if file.is_some() {
                return None;
            }
            file = Some(PathBuf::from(tok));
        }
    }

    let file = file?;
    Some(Commands::Run {
        file,
        pretty: true,
        lite,
        args,
        trust: false,
        backend: BackendArg::Auto,
        require_hash: false,
    })
}

fn main() -> miette::Result<()> {
    // v6-fix-D2-J: phase timing begins *before* `clap::Cli::parse`
    // so the `RELON_CLI_PROFILE=1` trace includes the parser cost
    // (previously hidden in the run-prologue total). The probe stays
    // gated on the env var so the production hot path pays nothing.
    let profile_main_entry = std::time::Instant::now();
    let profile_enabled = std::env::var_os("RELON_CLI_PROFILE").is_some();
    if profile_enabled {
        let entry_us = profile_main_entry.elapsed().as_micros();
        eprintln!(
            "[relon-cli profile] main_entry              +{entry_us:>6}us  (total {entry_us}us)"
        );
    }
    // v6-fix-D2-J: try the hand-rolled argv matcher first. On the W11
    // shape (`run <file> [--lite] [--args …]`) this skips clap's
    // ~270 µs cold-start tax entirely; any other argv shape falls
    // through to `Cli::parse()` so flag semantics stay clap-defined.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let command = match try_parse_run_fast(&argv) {
        Some(cmd) => {
            if profile_enabled {
                let total_us = profile_main_entry.elapsed().as_micros();
                eprintln!(
                    "[relon-cli profile] argv_fast_run          +{total_us:>6}us  (total {total_us}us)"
                );
            }
            cmd
        }
        None => {
            let cli = Cli::parse();
            if profile_enabled {
                let total_us = profile_main_entry.elapsed().as_micros();
                eprintln!(
                    "[relon-cli profile] clap::parse             +{total_us:>6}us  (total {total_us}us)"
                );
            }
            cli.command
        }
    };

    match command {
        Commands::Run {
            file,
            pretty,
            lite,
            args,
            trust,
            backend,
            require_hash,
        } => cmd_run(file, pretty, lite, args, trust, backend, require_hash)?,
        Commands::Fmt {
            check,
            stdout,
            files,
        } => {
            run_fmt(check, stdout, files)?;
        }
        #[cfg(feature = "cli-lsp")]
        Commands::Lsp => {
            relon_lsp::server::run_stdio()
                .map_err(|e| miette::miette!("language server exited with error: {e}"))?;
        }
    }

    Ok(())
}

/// Handle the `relon run` subcommand. Extracted from `main` so the
/// 670-line body lives in its own fn (the dispatch + every backend
/// arm stay together as before; future cleanups can split this
/// further by phase). Returns `Err` for any operator-facing failure
/// (parse / analyze / runtime / IO) — `main` propagates straight
/// through.
/// Warning emitted when `--trust` is passed but the resolved execution
/// path is the cranelift-AOT backend, which the CLI cannot honour it on
/// (no host-fn registry to grant capabilities from). Surfacing this
/// keeps the flag from being a silent no-op: tree-walk is the backend
/// that acts on `--trust`.
const TRUST_UNSUPPORTED_ON_AOT: &str =
    "[relon-cli] warning: --trust has no effect on the cranelift-AOT backend \
     (single file, no host-fn registry): it grants no runtime native-fn \
     capability, and the `#import` paths --trust would open are not stitched \
     by this backend. Use --backend tree-walk if your \
     program relies on --trust.";

fn warn_trust_unsupported_on_aot() {
    eprintln!("{TRUST_UNSUPPORTED_ON_AOT}");
}

fn parse_main_args_json(
    args_json: &str,
    signature: &MainSignature,
    tree: &AnalyzedTree,
) -> Result<HashMap<String, Value>, String> {
    let raw_args: serde_json::Map<String, JsonValue> =
        serde_json::from_str(args_json).map_err(|e| {
            format!("--args must be a JSON object keyed by `#main(...)` parameter names: {e}")
        })?;
    let param_types: HashMap<&str, &TypeNode> = signature
        .params
        .iter()
        .map(|param| (param.name.as_str(), &param.type_node))
        .collect();
    let mut args = HashMap::with_capacity(raw_args.len());
    for (name, json) in raw_args {
        let value = if let Some(type_hint) = param_types.get(name.as_str()) {
            let type_hint = *type_hint;
            decode_json_for_type(json, type_hint, tree).map_err(|e| {
                format!(
                    "--args value for `{name}` cannot be decoded as {}: {e}",
                    format_type(type_hint)
                )
            })?
        } else {
            targetless_json_to_value(json)?
        };
        args.insert(name, value);
    }
    Ok(args)
}

fn decode_json_for_type(
    json: JsonValue,
    type_hint: &TypeNode,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    if type_hint.is_optional {
        let mut inner_type = type_hint.clone();
        inner_type.is_optional = false;
        return decode_json_for_option(json, Some(&inner_type), tree);
    }

    if type_key(type_hint) == "Option" {
        return decode_json_for_option(json, type_hint.generics.first(), tree);
    }

    if type_key(type_hint) == "Result" {
        return decode_json_for_result(
            json,
            type_hint.generics.first(),
            type_hint.generics.get(1),
            tree,
        );
    }

    if let Some(value) = decode_json_for_builtin_scalar(json.clone(), type_hint) {
        return value;
    }

    match type_key(type_hint).as_str() {
        "Tuple" => decode_json_array_as_tuple_or_targetless(json, &type_hint.generics, tree),
        "List" => decode_json_array_as_list_or_targetless(json, type_hint.generics.first(), tree),
        "Dict" => decode_json_object_as_dict_or_targetless(json, type_hint.generics.get(1), tree),
        "Enum" => decode_json_for_enum(json, type_hint, tree),
        _ => {
            if let Some(def) = schema_def_for_type(type_hint, tree) {
                if !def.variants.is_empty() {
                    return decode_json_for_tagged_enum_schema(json, type_hint, def, tree);
                }
            }
            if let Some(elements) = schema_tuple_elements_for_type(type_hint, tree) {
                decode_json_array_as_tuple_or_targetless(json, &elements, tree)
            } else if let Some(fields) = schema_fields_for_type(type_hint, tree) {
                decode_json_object_as_schema_or_targetless(json, &fields, tree)
            } else {
                targetless_json_to_value(json)
            }
        }
    }
}

fn decode_json_for_builtin_scalar(
    json: JsonValue,
    type_hint: &TypeNode,
) -> Option<Result<Value, String>> {
    match type_key(type_hint).as_str() {
        "Any" => Some(targetless_json_to_value(json)),
        "Bool" => Some(match json {
            JsonValue::Bool(value) => Ok(Value::Bool(value)),
            other => Err(format!("expected JSON bool, got {}", json_kind(&other))),
        }),
        "Int" => Some(match json {
            JsonValue::Number(value) => value
                .as_i64()
                .map(Value::Int)
                .ok_or_else(|| "expected JSON integer in i64 range".to_string()),
            other => Err(format!("expected JSON integer, got {}", json_kind(&other))),
        }),
        "Float" => Some(match json {
            JsonValue::Number(value) => value
                .as_f64()
                .map(|value| Value::Float(value.into()))
                .ok_or_else(|| "expected finite JSON number".to_string()),
            other => Err(format!("expected JSON number, got {}", json_kind(&other))),
        }),
        "Number" => Some(match json {
            JsonValue::Number(value) => {
                if let Some(int) = value.as_i64() {
                    Ok(Value::Int(int))
                } else {
                    value
                        .as_f64()
                        .map(|value| Value::Float(value.into()))
                        .ok_or_else(|| "expected finite JSON number".to_string())
                }
            }
            other => Err(format!("expected JSON number, got {}", json_kind(&other))),
        }),
        "String" => Some(match json {
            JsonValue::String(value) => Ok(Value::String(value.into())),
            other => Err(format!("expected JSON string, got {}", json_kind(&other))),
        }),
        _ => None,
    }
}

fn json_kind(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn decode_json_for_option(
    json: JsonValue,
    inner_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    match json {
        JsonValue::Null => Ok(Value::option_none()),
        JsonValue::String(name) if name == "None" => Ok(Value::option_none()),
        JsonValue::Object(mut map) if map.len() == 1 && map.contains_key("None") => {
            let payload = map.remove("None").expect("checked None payload");
            match payload {
                JsonValue::Null => Ok(Value::option_none()),
                JsonValue::Object(obj) if obj.is_empty() => Ok(Value::option_none()),
                _ => Err("Option.None expects null or an empty object payload".to_string()),
            }
        }
        JsonValue::Object(mut map) if map.len() == 1 && map.contains_key("Some") => {
            let payload = map.remove("Some").expect("checked Some payload");
            decode_json_single_payload(payload, "value", inner_type, tree).map(Value::option_some)
        }
        other => {
            decode_json_single_payload(other, "value", inner_type, tree).map(Value::option_some)
        }
    }
}

fn decode_json_for_result(
    json: JsonValue,
    ok_type: Option<&TypeNode>,
    err_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    let JsonValue::Object(mut map) = json else {
        return Err(
            r#"Result<T, E> expects an externally tagged object: {"Ok": ...} or {"Err": ...}"#
                .to_string(),
        );
    };
    if map.len() != 1 {
        return Err("Result<T, E> expects exactly one variant key: Ok or Err".to_string());
    }
    if let Some(payload) = map.remove("Ok") {
        return decode_json_single_payload(payload, "value", ok_type, tree).map(Value::result_ok);
    }
    if let Some(payload) = map.remove("Err") {
        return decode_json_single_payload(payload, "error", err_type, tree).map(Value::result_err);
    }
    let name = map.keys().next().cloned().unwrap_or_default();
    Err(format!(
        "object key `{name}` is not a Result variant; expected Ok or Err"
    ))
}

fn decode_json_single_payload(
    payload: JsonValue,
    field_name: &str,
    field_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    let Some(field_type) = field_type else {
        return targetless_json_to_value(payload);
    };
    match decode_json_for_type(payload.clone(), field_type, tree) {
        Ok(value) => Ok(value),
        Err(direct_err) => match payload {
            JsonValue::Object(mut map) if map.len() == 1 && map.contains_key(field_name) => {
                let inner = map.remove(field_name).expect("checked payload field");
                decode_json_for_type(inner, field_type, tree)
            }
            _ => Err(direct_err),
        },
    }
}

fn decode_json_array_as_tuple_or_targetless(
    json: JsonValue,
    element_types: &[TypeNode],
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    match json {
        JsonValue::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for (idx, item) in items.into_iter().enumerate() {
                let value = match element_types.get(idx) {
                    Some(slot_type) => decode_json_for_type(item, slot_type, tree)?,
                    None => targetless_json_to_value(item)?,
                };
                values.push(value);
            }
            Ok(Value::tuple(values))
        }
        other => targetless_json_to_value(other),
    }
}

fn decode_json_array_as_list_or_targetless(
    json: JsonValue,
    item_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    match json {
        JsonValue::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                let value = match item_type {
                    Some(item_type) => decode_json_for_type(item, item_type, tree)?,
                    None => targetless_json_to_value(item)?,
                };
                values.push(value);
            }
            Ok(Value::list(values))
        }
        other => targetless_json_to_value(other),
    }
}

fn decode_json_object_as_dict_or_targetless(
    json: JsonValue,
    value_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    match json {
        JsonValue::Object(map) => {
            let mut values = Vec::with_capacity(map.len());
            for (key, item) in map {
                let value = match value_type {
                    Some(value_type) => decode_json_for_type(item, value_type, tree)?,
                    None => targetless_json_to_value(item)?,
                };
                values.push((key, value));
            }
            Ok(Value::dict(values))
        }
        other => targetless_json_to_value(other),
    }
}

fn decode_json_object_as_schema_or_targetless(
    json: JsonValue,
    field_types: &HashMap<String, TypeNode>,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    match json {
        JsonValue::Object(map) => {
            let mut values = Vec::with_capacity(map.len());
            for (key, item) in map {
                let value = match field_types.get(&key) {
                    Some(field_type) => decode_json_for_type(item, field_type, tree)?,
                    None => targetless_json_to_value(item)?,
                };
                values.push((key, value));
            }
            Ok(Value::dict(values))
        }
        other => targetless_json_to_value(other),
    }
}

fn decode_json_for_tagged_enum_schema(
    json: JsonValue,
    type_hint: &TypeNode,
    def: &SchemaDef,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    let enum_name = def.name.clone().unwrap_or_else(|| type_key(type_hint));
    match json {
        JsonValue::String(name) => decode_json_string_as_enum_unit(name, &enum_name, def),
        JsonValue::Object(map) => {
            if map.len() != 1 {
                return Err(format!(
                    "enum `{enum_name}` expects an externally tagged object with exactly one variant key"
                ));
            }
            let (variant_name, payload) = map
                .into_iter()
                .next()
                .expect("checked exactly one externally tagged enum entry");
            decode_json_object_as_enum_variant(variant_name, payload, type_hint, def, tree)
        }
        other => targetless_json_to_value(other),
    }
}

fn decode_json_string_as_enum_unit(
    name: String,
    enum_name: &str,
    def: &SchemaDef,
) -> Result<Value, String> {
    let matching: Vec<_> = def
        .variants
        .iter()
        .filter(|variant| variant.name == name)
        .collect();
    match matching.as_slice() {
        [variant] if variant.fields.is_empty() => Ok(Value::variant_dict(
            Vec::<(String, Value)>::new(),
            variant.name.clone(),
            enum_name.to_string(),
        )),
        [variant] => Err(format!(
            "string `{}` names enum variant `{}` but that variant requires payload fields",
            name, variant.name
        )),
        [] => Err(format!(
            "string `{name}` does not name a unit variant of enum `{enum_name}`"
        )),
        _ => Err(format!(
            "string `{name}` is ambiguous for enum `{enum_name}`"
        )),
    }
}

fn decode_json_object_as_enum_variant(
    variant_name: String,
    payload: JsonValue,
    type_hint: &TypeNode,
    def: &SchemaDef,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    let enum_name = def.name.clone().unwrap_or_else(|| type_key(type_hint));
    let variant = def
        .variants
        .iter()
        .find(|variant| variant.name == variant_name)
        .ok_or_else(|| {
            format!("object key `{variant_name}` is not a variant of enum `{enum_name}`")
        })?;
    let fields = substituted_variant_fields(variant, def, &type_hint.generics)?;
    if fields.is_empty() {
        return match payload {
            JsonValue::Object(map) if map.is_empty() => Ok(Value::variant_dict(
                Vec::<(String, Value)>::new(),
                variant.name.clone(),
                enum_name,
            )),
            _ => Err(format!(
                "unit enum variant `{}` expects an empty object payload",
                variant.name
            )),
        };
    }

    let values = if variant_fields_are_tuple_payload(&fields) {
        decode_json_payload_as_tuple_variant(payload, &fields, tree, &variant.name)?
    } else {
        decode_json_payload_as_struct_variant(payload, &fields, tree, &variant.name)?
    };
    Ok(Value::variant_dict(values, variant.name.clone(), enum_name))
}

fn substituted_variant_fields(
    variant: &relon_analyzer::schema::EnumVariant,
    def: &SchemaDef,
    concrete_args: &[TypeNode],
) -> Result<Vec<(String, TypeNode)>, String> {
    variant
        .fields
        .iter()
        .map(|field| {
            let type_hint = field.type_hint.as_ref().ok_or_else(|| {
                format!(
                    "enum variant `{}` field `{}` has no static type",
                    variant.name, field.name
                )
            })?;
            Ok((
                field.name.clone(),
                substitute_schema_type(type_hint, &def.generics, concrete_args),
            ))
        })
        .collect()
}

fn variant_fields_are_tuple_payload(fields: &[(String, TypeNode)]) -> bool {
    !fields.is_empty()
        && fields
            .iter()
            .enumerate()
            .all(|(idx, (name, _))| *name == idx.to_string())
}

fn decode_json_payload_as_tuple_variant(
    payload: JsonValue,
    fields: &[(String, TypeNode)],
    tree: &AnalyzedTree,
    variant_name: &str,
) -> Result<Vec<(String, Value)>, String> {
    let JsonValue::Array(items) = payload else {
        return Err(format!(
            "tuple enum variant `{variant_name}` expects a JSON array payload"
        ));
    };
    if items.len() != fields.len() {
        return Err(format!(
            "tuple enum variant `{variant_name}` expects {} elements, got {}",
            fields.len(),
            items.len()
        ));
    }
    fields
        .iter()
        .zip(items)
        .map(|((name, field_type), item)| {
            decode_json_for_type(item, field_type, tree).map(|value| (name.clone(), value))
        })
        .collect()
}

fn decode_json_payload_as_struct_variant(
    payload: JsonValue,
    fields: &[(String, TypeNode)],
    tree: &AnalyzedTree,
    variant_name: &str,
) -> Result<Vec<(String, Value)>, String> {
    let JsonValue::Object(mut map) = payload else {
        return Err(format!(
            "struct enum variant `{variant_name}` expects a JSON object payload"
        ));
    };
    let mut values = Vec::with_capacity(fields.len());
    for (name, field_type) in fields {
        let item = map
            .remove(name)
            .ok_or_else(|| format!("enum variant `{variant_name}` is missing field `{name}`"))?;
        values.push((name.clone(), decode_json_for_type(item, field_type, tree)?));
    }
    if !map.is_empty() {
        let mut names: Vec<_> = map.keys().cloned().collect();
        names.sort();
        return Err(format!(
            "enum variant `{variant_name}` received unknown field(s): {}",
            names.join(", ")
        ));
    }
    Ok(values)
}

fn decode_json_for_enum(
    json: JsonValue,
    type_hint: &TypeNode,
    tree: &AnalyzedTree,
) -> Result<Value, String> {
    let arity = match &json {
        JsonValue::Array(items) => items.len(),
        _ => return targetless_json_to_value(json),
    };
    let matching_tuple_alts: Vec<&TypeNode> = type_hint
        .generics
        .iter()
        .filter(|alt| type_accepts_tuple_array(alt, arity, tree))
        .collect();
    if matching_tuple_alts.len() == 1 {
        decode_json_for_type(json, matching_tuple_alts[0], tree)
    } else {
        targetless_json_to_value(json)
    }
}

fn type_accepts_tuple_array(type_hint: &TypeNode, arity: usize, tree: &AnalyzedTree) -> bool {
    if type_key(type_hint) == "Tuple" {
        return type_hint.generics.len() == arity;
    }
    matches!(
        schema_tuple_elements_for_type(type_hint, tree),
        Some(elements) if elements.len() == arity
    )
}

fn targetless_json_to_value(json: JsonValue) -> Result<Value, String> {
    if json.is_null() {
        return Err("JSON null needs an Option<T> target type".to_string());
    }
    serde_json::from_value::<Value>(json).map_err(|e| format!("invalid JSON value: {e}"))
}

fn schema_tuple_elements_for_type(
    type_hint: &TypeNode,
    tree: &AnalyzedTree,
) -> Option<Vec<TypeNode>> {
    if let Some(def) = local_schema_def_for_type(type_hint, tree) {
        if let Some(elements) = &def.tuple_elements {
            return Some(substitute_schema_types(
                elements,
                &def.generics,
                &type_hint.generics,
            ));
        }
    }
    let key = type_key(type_hint);
    let imports = tree.workspace_import_index.as_ref()?;
    let elements = imports.imported_tuple_schemas.get(&key)?;
    let generics = imports
        .imported_schema_generics
        .get(&key)
        .cloned()
        .unwrap_or_default();
    Some(substitute_schema_types(
        elements,
        &generics,
        &type_hint.generics,
    ))
}

fn schema_fields_for_type(
    type_hint: &TypeNode,
    tree: &AnalyzedTree,
) -> Option<HashMap<String, TypeNode>> {
    if let Some(def) = local_schema_def_for_type(type_hint, tree) {
        return Some(substitute_schema_fields_from_def(def, &type_hint.generics));
    }
    let key = type_key(type_hint);
    let imports = tree.workspace_import_index.as_ref()?;
    let fields = imports.imported_schemas.get(&key)?;
    let generics = imports
        .imported_schema_generics
        .get(&key)
        .cloned()
        .unwrap_or_default();
    Some(substitute_schema_fields(
        fields,
        &generics,
        &type_hint.generics,
    ))
}

fn local_schema_def_for_type<'a>(
    type_hint: &TypeNode,
    tree: &'a AnalyzedTree,
) -> Option<&'a SchemaDef> {
    if type_hint.path.len() != 1 {
        return None;
    }
    let name = &type_hint.path[0];
    let decl = tree.root_schemas.iter().find(|decl| &decl.name == name)?;
    tree.schemas.get(&decl.schema_node.id)
}

fn schema_def_for_type<'a>(type_hint: &TypeNode, tree: &'a AnalyzedTree) -> Option<&'a SchemaDef> {
    if let Some(def) = local_schema_def_for_type(type_hint, tree) {
        return Some(def);
    }
    let key = type_key(type_hint);
    tree.workspace_import_index
        .as_ref()?
        .imported_schema_defs
        .get(&key)
}

fn substitute_schema_fields_from_def(
    def: &SchemaDef,
    concrete_args: &[TypeNode],
) -> HashMap<String, TypeNode> {
    let fields = def.fields.iter().filter_map(|field| {
        let type_hint = field.type_hint.as_ref()?;
        Some((field.name.clone(), type_hint.clone()))
    });
    substitute_schema_fields_from_iter(fields, &def.generics, concrete_args)
}

fn substitute_schema_fields(
    fields: &HashMap<String, TypeNode>,
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> HashMap<String, TypeNode> {
    substitute_schema_fields_from_iter(
        fields
            .iter()
            .map(|(name, type_hint)| (name.clone(), type_hint.clone())),
        generic_names,
        concrete_args,
    )
}

fn substitute_schema_fields_from_iter(
    fields: impl Iterator<Item = (String, TypeNode)>,
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> HashMap<String, TypeNode> {
    fields
        .map(|(name, type_hint)| {
            (
                name,
                substitute_schema_type(&type_hint, generic_names, concrete_args),
            )
        })
        .collect()
}

fn substitute_schema_types(
    types: &[TypeNode],
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> Vec<TypeNode> {
    types
        .iter()
        .map(|type_hint| substitute_schema_type(type_hint, generic_names, concrete_args))
        .collect()
}

fn substitute_schema_type(
    type_hint: &TypeNode,
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> TypeNode {
    let subst: HashMap<String, TypeNode> = generic_names
        .iter()
        .cloned()
        .zip(concrete_args.iter().cloned())
        .collect();
    if subst.is_empty() {
        type_hint.clone()
    } else {
        substitute_generics_in_typenode(type_hint, &subst)
    }
}

fn type_key(type_hint: &TypeNode) -> String {
    type_hint.path.join(".")
}

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    file: PathBuf,
    pretty: bool,
    lite: bool,
    args: Option<String>,
    trust: bool,
    backend: BackendArg,
    require_hash: bool,
) -> miette::Result<()> {
    // v6-fix-D2: phase timing. Emitted only when the
    // operator opts in via `RELON_CLI_PROFILE=1`; default
    // path stays silent so `--lite` doesn't pollute
    // stderr.
    let profile_phases = std::env::var_os("RELON_CLI_PROFILE").is_some();
    let phase_start = std::time::Instant::now();
    let mut last_phase_at = phase_start;
    let mut phase = |label: &str| {
        if !profile_phases {
            return;
        }
        let now = std::time::Instant::now();
        let total_us = now.duration_since(phase_start).as_micros();
        let delta_us = now.duration_since(last_phase_at).as_micros();
        eprintln!("[relon-cli profile] {label:<24} +{delta_us:>6}us  (total {total_us}us)");
        last_phase_at = now;
    };
    // v6-fix-D2: `--lite` forces the tree-walker, which is
    // the only backend whose cold-start fits inside a 2x
    // LuaJIT envelope on the W11 shape. Resolve the effective
    // backend once here so the dispatch site below has a
    // single source of truth:
    //   * --lite + cranelift-aot            -> hard reject
    //     (operator should not see a silent swap).
    //   * --lite + auto / tree-walk         -> tree-walk
    //   * no --lite                          -> requested backend
    let effective_backend = if lite {
        match backend {
            BackendArg::Auto | BackendArg::TreeWalk => BackendArg::TreeWalk,
            other => {
                return Err(miette::miette!(
                            "--lite forces `--backend tree-walk`; remove the explicit `--backend {:?}` or drop `--lite`",
                            other
                        ));
            }
        }
    } else {
        backend
    };
    let canonical_file = std::fs::canonicalize(&file)
        .into_diagnostic()
        .map_err(|e| e.wrap_err(format!("Failed to resolve file {:?}", file)))?;
    phase("canonicalize");
    let content = std::fs::read_to_string(&canonical_file)
        .into_diagnostic()
        .map_err(|e| e.wrap_err(format!("Failed to read file {:?}", canonical_file)))?;
    phase("read_to_string");

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
    // v3++ b-2: forward the `--require-hash` policy so the
    // workspace pass can flag unpinned remote imports up front
    // (before the loader runs). All other analyzer knobs stay
    // at their defaults so this branch keeps its prior behavior.
    // v6-fix-D2 default-path-2: detect trivial scalar
    // `#main(...)` shapes (single Int/Float/Bool/String
    // param + literal-or-arith body, no `#import`) so the
    // default branch can take the same fast-analyze path
    // `--lite` does. The classifier reads the raw source
    // text — cheap, no analyzer state required — and the
    // tree-walker's per-method-dispatch surface never enters
    // trivial-`#main` bodies, so skipping carrier injection
    // is observationally invisible.
    //
    // v6-fix-D2-I (this stage): replace the previous
    // double-parse — first inside `is_trivial_scalar_main`,
    // then again inside the lite branch's
    // `relon_parser::parse_document` — with a single attempt
    // at the cold-start fast path. `parse_document_fast` only
    // accepts the trivial-scalar `#main` envelope; success
    // both confirms triviality *and* yields the parsed Node
    // the lite branch needs. Cache the result here so neither
    // the classifier nor the lite-parse pays a second parse.
    let prelite_node = if !lite {
        relon_parser::parse_document_fast(&content)
    } else {
        // `--lite` already opts the operator into the
        // tree-walker; still try the fast path so the
        // explicit `--lite` invocation also avoids the
        // rowan CST construction on trivial sources.
        relon_parser::parse_document_fast(&content)
    };
    let trivial_default = !lite
        && match &prelite_node {
            Some(node) => relon::is_trivial_scalar_main_node(node),
            None => relon::is_trivial_scalar_main(&content),
        };
    phase("trivial_classify");
    let lite_analyze = lite || trivial_default;
    let analyze_options = AnalyzeOptions {
        require_hash,
        // v6-fix-D2 cold-start: `--lite` strips the
        // built-in `core/*.relon` carrier pass from the
        // analyzer. The carrier only feeds the
        // `s.upper()` / `lst.map(f)`-style method
        // dispatch table — it's safe to skip on entries
        // that don't invoke built-in methods. The
        // operator opts in by passing `--lite`; any
        // method call against a built-in carrier in a
        // `--lite` run will surface as
        // `UnknownMethod`. (See `--lite`'s flag doc for
        // the contract.)
        //
        // v6-fix-D2 default-path: trivial scalar `#main`
        // bodies (path b) provably do not dispatch any
        // built-in method. Skip the carrier in that
        // branch too — same fast-analyze the operator
        // would get from `--lite`, without making them
        // pass the flag.
        skip_core_schemas: lite_analyze,
        // v6-fix-D2-H cold-start: when the lite/trivial
        // path is selected, also permit the analyzer's
        // trivial-`#main` fast-path so passes that are
        // provable no-ops on this shape (schema-collect,
        // extend, constraints, resolve, full typecheck)
        // drop out. The analyzer re-validates the shape
        // internally and falls through to the full
        // pipeline on any non-trivial source, so flipping
        // this on for every lite-mode entry is safe.
        trivial_main_fast_path: lite_analyze,
        ..AnalyzeOptions::default()
    };
    // v6-fix-D2 cold-start fast path. The default
    // workspace pass (`analyze_entry_with_options`) is the
    // single biggest in-process cost for short-lived
    // invocations (~1.8 ms on a `#main(Int x) -> Int\nx + 1`
    // shape). Most of that goes into BFS bookkeeping, cycle
    // detection, cross-module schema collision, and the
    // unknown-types re-check — every one of which is a
    // no-op when the entry source has no `#import`
    // directive. `--lite` short-circuits by parsing +
    // running the per-module `analyze_with_options` pass
    // directly, then hand-rolling a `WorkspaceTree` that
    // looks identical to the multi-pass output for an
    // import-free entry. Import-bearing entries fall back
    // to the full workspace path so cross-module diagnostics
    // keep their teeth.
    let workspace = if lite_analyze {
        // v6-fix-D2-I: reuse the fast-path Node if the
        // pre-classify attempt accepted; otherwise fall back
        // to the rowan/CST `parse_document`. Either way the
        // lite branch sees one parsed `Node` — no double-parse.
        let parsed = match prelite_node {
            Some(node) => Ok(node),
            None => relon_parser::parse_document(&content),
        };
        match parsed {
            Ok(node) => {
                phase("[lite] parse_only");
                let arc_node = Arc::new(node);
                // Mirror `workspace_build::build`'s entry
                // strict-mode decision: workspace flag AND
                // no `#relaxed` / `#unstrict` directive on
                // the root. The directive constants come
                // from `relon_parser::directive` so the
                // CLI stays in lockstep with the analyzer
                // crate's view.
                let entry_relaxed = arc_node.directives.iter().any(|d| {
                    d.name == relon_parser::directive::RELAXED
                        || d.name == relon_parser::directive::UNSTRICT
                });
                let entry_strict = analyze_options.strict_mode && !entry_relaxed;
                let mut eff = analyze_options.clone();
                eff.strict_mode = entry_strict;
                let tree = analyze_with_options(&arc_node, &eff);
                phase("[lite] analyze");
                // Only short-circuit when there really are
                // no imports. Otherwise the cross-module
                // analysis the workspace pass owns must
                // run; fall back to the standard path.
                if tree.imports.is_empty() {
                    let mut ws = WorkspaceTree::new();
                    ws.entry_id = cache_namespace.clone();
                    ws.strict_mode = entry_strict;
                    ws.import_graph.insert(cache_namespace.clone(), Vec::new());
                    ws.modules.insert(cache_namespace.clone(), Arc::new(tree));
                    ws.nodes.insert(cache_namespace.clone(), arc_node);
                    phase("[lite] synth_ws");
                    ws
                } else {
                    phase("[lite] has_imports_fallback");
                    analyze_entry_with_options(
                        cache_namespace.clone(),
                        &content,
                        entry_dir,
                        &mut loader,
                        &analyze_options,
                    )
                }
            }
            Err(parse_err) => {
                // Synthesize a workspace-shaped parse-error
                // record so the downstream error renderer
                // (which expects a `WorkspaceTree`) still
                // gets a coherent input. Mirrors the
                // workspace_build path verbatim.
                phase("[lite] parse_failed");
                let mut ws = WorkspaceTree::new();
                ws.entry_id = cache_namespace.clone();
                ws.workspace_diagnostics
                    .push(WorkspaceDiagnostic::ModuleParseError {
                        path: cache_namespace.clone(),
                        message: parse_err.to_string(),
                        range: miette::SourceSpan::from((0usize, 0usize)),
                    });
                ws
            }
        }
    } else {
        if profile_phases {
            let _ = relon_parser::parse_document(&content);
            phase("[probe] parse_only");
        }
        let ws = analyze_entry_with_options(
            cache_namespace.clone(),
            &content,
            entry_dir,
            &mut loader,
            &analyze_options,
        );
        phase("analyze_entry");
        ws
    };
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
                        path, message, ..
                    } if path == &cache_namespace => Some(message.clone()),
                    _ => None,
                })
        {
            if let Err(e) = parse_document(&content) {
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
                return Err(
                    report.with_source_code(NamedSource::new(file.to_string_lossy(), content))
                );
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
            // v3+ a-3: --trust opens remote `#import "https://..."`
            // resolution. Resolver lives on native targets only;
            // the CLI never runs on wasm32 so an unconditional
            // mount is safe.
            //
            // Phase G.W11 Phase 2: the resolver is gated behind the
            // `remote-http` cargo feature. Default CLI builds drop
            // ureq + rustls + ring (~700 KB text) so the W11
            // trivial-`#main` cold start stays lean. Operators that
            // need remote `#import` rebuild with
            // `cargo build --features cli-remote-http`. With the
            // feature off, the `ResolverChainLoader` still emits
            // `RemoteImportDenied` for `https://` paths so the
            // failure mode is unchanged from a sandboxed run.
            #[cfg(feature = "cli-remote-http")]
            ctx.prepend_module_resolver(Arc::new(
                relon_evaluator::module::RemoteHttpResolver::new(),
            ));
        }
        Arc::new({
            let mut ctx = ctx;
            // v6-fix-D2-I: the trivial-scalar `#main` envelope
            // never dispatches into stdlib / decorators /
            // prelude — `is_trivial_scalar_main` rejects any
            // fn call, dict literal, schema, `#import`. Skip
            // the ~86 HashMap inserts + Arc allocations the
            // full `prepare_in_place` performs on the trivial
            // path. Both `lite` (operator opt-in) and
            // `trivial_default` (auto-detected) go through
            // the same classifier guard, so they share the
            // lite-prep shortcut.
            if lite_analyze {
                relon_evaluator::TreeWalkEvaluator::prepare_in_place_lite(&mut ctx);
            } else {
                relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
            }
            ctx
        })
    };
    let _root_loading_guard = ctx.enter_loading_module(cache_namespace.clone());
    let evaluator = TreeWalkEvaluator::new(Arc::clone(&ctx));
    phase("context+tw_evaluator");

    let scope = std::sync::Arc::new(Scope {
        current_dir: canonical_file
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_string_lossy()
            .into_owned()
            .into(),
        cache_namespace: cache_namespace.clone().into(),
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
        let entry_tree = workspace
            .modules
            .get(&cache_namespace)
            .expect("workspace passed has_main check but entry tree missing");
        let signature = entry_tree
            .main_signature
            .as_ref()
            .expect("workspace passed has_main check but entry signature missing");
        let args_map = parse_main_args_json(&args_json, signature, entry_tree).map_err(|e| {
            miette::miette!("{e}")
                .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
        })?;
        // `--backend auto` (default) flows through
        // [`relon::AutoEvaluator`], which (a) probes the
        // on-disk cache (`from_cache_dir` → dlopen-execute)
        // before falling back to `from_source_with_cache`
        // (writes a fresh cache pair as a side-effect), and
        // (b) short-circuits straight to the tree-walker
        // when the source is a trivial scalar `#main` (no
        // cranelift cold-start at all). The second `relon
        // run` of the same source therefore lands on the
        // dlopen path — closing the lap on
        // `--lite`'s tree-walker time. Note: `--trust` does not
        // affect the cranelift-AOT leg of this path (see the
        // `BackendArg::CraneliftAot` arm for why); the only
        // trivial-scalar sources routed here carry no guarded
        // `#native` op, so trust is a no-op on those shapes.
        //
        // v6-fix-D2: `--lite` forces tree-walk for the
        // `#main(...)` path too. The lower / JIT path
        // (cranelift-AOT) dominates the W11 cold-start
        // budget; the tree-walker reaches the same
        // scalar answer with parse + analyze + walk
        // only.
        phase("backend_select");
        match effective_backend {
            BackendArg::Auto => {
                // v6-fix-D2 default-path. Two short-circuits
                // apply here, in priority order:
                //
                // (b) Trivial scalar `#main` (single scalar
                // param + literal / arith body): route
                // straight through the workspace-built
                // tree-walker. The cranelift cold-start
                // (~4-5 ms) is pure overhead on these
                // shapes; the tree-walker reaches the same
                // scalar answer in ~60 µs. We share the
                // already-prepared `evaluator` + workspace
                // so the duplicate parse + analyze that
                // `AutoEvaluator::new` would have done stays
                // out of the hot path.
                //
                // (a) Otherwise drive the cranelift-AOT
                // pipeline via `from_source_with_cache` so
                // the second cold start of the same source
                // can dlopen-execute the cached binary
                // (skips parse + analyze + lower entirely).
                // The cache directory is the conventional
                // `$XDG_CACHE_HOME/relon` (override via env
                // for test isolation).
                if trivial_default {
                    phase("default_trivial_tree_walk");
                    evaluator.run_main(&scope, args_map).map_err(|e| {
                        Report::new(e).with_source_code(NamedSource::new(
                            file.to_string_lossy(),
                            content.clone(),
                        ))
                    })?
                } else {
                    // Non-trivial `auto` routes through cranelift-AOT,
                    // which does not honour `--trust` (see the
                    // `CraneliftAot` arm) — warn rather than silently drop.
                    if trust {
                        warn_trust_unsupported_on_aot();
                    }
                    let cache_dir = relon_codegen_cranelift::default_cache_dir();
                    // Cache-hit fast path: pull a matching
                    // pair off disk if present. Any soft
                    // miss (file absent, integrity failure,
                    // metadata mismatch) returns `Ok(None)`;
                    // a hard I/O failure surfaces as a
                    // logged warning and we fall back to
                    // the source path.
                    let aot_opt = match AotEvaluator::from_cache_dir(&content, &cache_dir) {
                        Ok(opt) => opt,
                        Err(e) => {
                            // Cache infrastructure problem
                            // (not a miss). Log and fall
                            // back to source; we don't
                            // gate the live invocation on
                            // a transient cache issue. The
                            // line goes to stderr so a
                            // tracing subscriber installed
                            // by `RELON_LOG=...` upstream
                            // would already capture it via
                            // the codegen-cranelift
                            // `tracing::warn!` mirrored
                            // there.
                            eprintln!(
                                        "[relon-cli] AOT cache load failed: {e}; falling back to from_source"
                                    );
                            None
                        }
                    };
                    phase("default_cache_probe");
                    // Build the compiled backend (cache hit, or fresh
                    // from source). On a cache miss the source build can
                    // fail because the compiled backend can't *express*
                    // this `#main` shape (e.g. `-> List<P>`) — that is a
                    // capability boundary, not a program error, so auto
                    // adapts by falling back to the tree-walk oracle
                    // instead of surfacing the error. Genuine source
                    // errors (parse / analyze) and host / infra faults
                    // (JIT setup, module define, cache I/O) are *not*
                    // swallowed: they re-surface so the user sees the
                    // real problem. See `CraneliftError::is_unsupported_shape`.
                    let aot = match aot_opt {
                        Some(a) => Some(a),
                        None => match AotEvaluator::from_source_with_cache(&content, &cache_dir) {
                            Ok(a) => Some(a),
                            Err(e) if e.is_unsupported_shape() => {
                                // Observable: the user just lost AOT
                                // acceleration for this run. Mirror the
                                // existing `[relon-cli] ...` stderr style
                                // so a plain run still shows it.
                                eprintln!(
                                    "[relon-cli] auto backend: compiled (cranelift-AOT) path can't \
                                     express this #main shape ({e}); falling back to the tree-walk \
                                     interpreter — this run forgoes AOT acceleration"
                                );
                                phase("default_unsupported_tree_walk");
                                None
                            }
                            Err(e) => {
                                return Err(miette::miette!("auto backend setup: {e}")
                                    .with_source_code(NamedSource::new(
                                        file.to_string_lossy(),
                                        content.clone(),
                                    )));
                            }
                        },
                    };
                    match aot {
                        Some(aot) => EvaluatorTrait::run_main(&aot, args_map).map_err(|e| {
                            Report::new(e).with_source_code(NamedSource::new(
                                file.to_string_lossy(),
                                content.clone(),
                            ))
                        })?,
                        // Fallback: tree-walk is the golden oracle. It
                        // produces the same result the compiled path
                        // would have, or surfaces the genuine runtime /
                        // source error if the program is actually wrong.
                        None => evaluator.run_main(&scope, args_map).map_err(|e| {
                            Report::new(e).with_source_code(NamedSource::new(
                                file.to_string_lossy(),
                                content.clone(),
                            ))
                        })?,
                    }
                }
            }
            BackendArg::TreeWalk => evaluator.run_main(&scope, args_map).map_err(|e| {
                Report::new(e)
                    .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
            })?,
            BackendArg::CraneliftAot => {
                // Cranelift-AOT lowers the entry file through
                // the cranelift JIT and dispatches the same
                // host-args HashMap through `run_main`. We
                // build from source here (single-file entry);
                // workspace-aware multi-file imports remain
                // a v5-γ target for the cranelift backend.
                //
                // `--trust` cannot be honoured on this backend
                // yet: cranelift gates a guarded `#native` call by
                // looking the host fn up in the `CapabilityVtable`
                // (a `cap_bit → HostFnPtr` map) and trapping
                // `CapabilityDenied` on a null slot. Granting a
                // capability therefore means *registering a host
                // fn* via `install_capabilities_mut` /
                // `register_via_gate`, and the CLI ships no host-fn
                // registry — so there is nothing to grant. The
                // scalar `#main` envelope the CLI routes here also
                // lowers to no `CallNative` / `CheckCap` op, so the
                // gate never fires regardless of the trust flag.
                // Wiring `--trust` through requires a host-fn
                // registry plus a gate-driven vtable builder (a
                // cross-crate API addition tracked separately); we
                // deliberately do not fabricate an all-granted but
                // empty vtable here because installing it would
                // change no behaviour.
                // Surface the no-op rather than silently dropping the flag.
                if trust {
                    warn_trust_unsupported_on_aot();
                }
                let aot = AotEvaluator::from_source(&content).map_err(|e| {
                    miette::miette!("cranelift-aot backend setup: {e}")
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
                })?;
                EvaluatorTrait::run_main(&aot, args_map).map_err(|e| {
                    Report::new(e)
                        .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
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
        if matches!(backend, BackendArg::CraneliftAot) {
            return Err(miette::miette!(
                "cranelift-aot backend only supports `#main(...)` entries; the file declares no signature"
            )
            .with_source_code(NamedSource::new(file.to_string_lossy(), content)));
        }
        // `Auto` for library-mode falls through to the tree-walker
        // — cranelift-AOT can't run `eval_root`, so the auto-tier
        // rule ("AOT only on run_main") makes the right choice for free.
        evaluator.eval_root(&scope).map_err(|e| {
            Report::new(e)
                .with_source_code(NamedSource::new(file.to_string_lossy(), content.clone()))
        })?
    };

    phase("evaluate");
    let final_val = relon::to_json_value(result).map_err(|e| {
        miette::miette!("{}", e).with_source_code(NamedSource::new(file.to_string_lossy(), content))
    })?;
    phase("to_json_value");

    let output = if pretty {
        serde_json::to_string_pretty(&final_val).into_diagnostic()?
    } else {
        serde_json::to_string(&final_val).into_diagnostic()?
    };
    phase("serialise_json");

    println!("{}", output);
    phase("println");
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
