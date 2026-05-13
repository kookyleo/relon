//! WASM bindings for the Relon language.
//!
//! Exposes `evaluate` / `format` / `version` to JavaScript through
//! `wasm-bindgen`, intended for the docs playground (and any other
//! browser-side embedder that wants a Relon runtime without a server).
//!
//! Trust posture: **sandboxed only**. Filesystem `#import` and every
//! capability-gated host function are denied. Scripts can `#import "std/*"`
//! virtual modules and any other module the host supplied through the
//! `sources` map. Surfacing a `--trust` toggle to untrusted browser users
//! is intentionally out of scope.
//!
//! Errors returned to JS are structured `ErrorReport` JSON values rather
//! than opaque strings, so the playground can render gutter markers /
//! tooltips without re-parsing miette's text output.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use miette::Diagnostic as MietteDiagnostic;
use relon::ResolverChainLoader;
use relon_analyzer::{analyze_entry, Severity};
use relon_evaluator::module::{ModuleResolver, ModuleSource, StdModuleResolver};
use relon_evaluator::{Context, Evaluator, RuntimeError, Scope, Value};
use relon_parser::{parse_document, TokenRange};
use serde::{Deserialize, Serialize};
// Re-imported below where `value.serialize(&serializer)` is invoked; the
// `use serde::Serialize` already in scope satisfies the trait bound.
use wasm_bindgen::prelude::*;

/// Structured error payload returned to JavaScript. Stable JSON shape so
/// the playground UI can render markers without re-parsing text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorReport {
    /// Coarse kind so the UI can pick a colour / icon without inspecting
    /// the message.
    pub kind: ErrorKind,
    /// Human-readable summary. For analyzer batches this is the joined
    /// per-diagnostic header; for parse / eval errors the underlying
    /// `Display` impl.
    pub message: String,
    /// Source spans the error attaches to. May be empty when the error
    /// has no positional anchor (e.g. workspace-level cycle reports).
    pub spans: Vec<SpanInfo>,
    /// `miette`-style help text when available.
    pub help: Option<String>,
    /// Diagnostic code (e.g. `relon::analyze::unresolved_reference`) when
    /// the underlying error carries one. UI may use this for deep links.
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ErrorKind {
    /// `serde_json` failed to decode the `sources` argument or the
    /// arguments were structurally wrong (missing entry, etc.).
    InvalidInput,
    /// Entry-level parser error before the workspace pass even ran.
    ParseError,
    /// Analyzer reported at least one `Severity::Error` diagnostic in
    /// the workspace (entry or any imported module).
    AnalyzeError,
    /// Evaluator surfaced a `RuntimeError` at evaluate time.
    EvalError,
    /// JSON projection failed (non-finite float, closure, schema-only
    /// value, etc.). Distinguished from `EvalError` because the
    /// evaluation itself succeeded — only output conversion failed.
    ProjectionError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanInfo {
    /// Module the span belongs to, or `null` if the underlying
    /// diagnostic didn't carry one (workspace-level reports). The
    /// playground uses this to route the marker back to the right tab.
    pub file: Option<String>,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
    /// Optional label printed alongside the span by miette.
    pub label: Option<String>,
}

impl ErrorReport {
    fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidInput,
            message: message.into(),
            spans: Vec::new(),
            help: None,
            code: None,
        }
    }

    fn from_parse(message: String) -> Self {
        Self {
            kind: ErrorKind::ParseError,
            message,
            spans: Vec::new(),
            help: None,
            code: None,
        }
    }

    fn from_miette<D>(kind: ErrorKind, diag: &D, file: Option<&str>) -> Self
    where
        D: MietteDiagnostic + std::fmt::Display,
    {
        let mut spans = Vec::new();
        if let Some(labels) = diag.labels() {
            for label in labels {
                let span = label.inner();
                spans.push(SpanInfo {
                    file: file.map(|s| s.to_string()),
                    start: span.offset(),
                    end: span.offset() + span.len(),
                    label: label.label().map(|s| s.to_string()),
                });
            }
        }
        Self {
            kind,
            message: diag.to_string(),
            spans,
            help: diag.help().map(|c| c.to_string()),
            code: diag.code().map(|c| c.to_string()),
        }
    }
}

fn err_to_js(report: ErrorReport) -> JsValue {
    // Fallback to the message string if serialization itself fails, so
    // the JS side always sees *something* throwable rather than an
    // opaque `undefined`. Serialization should never fail in practice
    // (the struct is plain data), but defensive code is cheap here.
    //
    // `serialize_maps_as_objects(true)` matters here too: `spans` is a
    // `Vec<SpanInfo>` (already a JS array), but the wrapping struct is
    // serialised as a JS object — without the flag, downstream
    // `err.kind` access on the JS side would fail.
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    match report.serialize(&serializer) {
        Ok(value) => value,
        Err(_) => JsValue::from_str(&report.message),
    }
}

/// In-memory resolver: feeds the `Map<path, content>` the browser
/// supplied into the import pipeline. Lookups are exact-string against
/// the host map, then a join with `scope.current_dir` for relative
/// imports (`#import "./lib.relon"`). No `std::fs` calls — safe on
/// wasm32 even in a `wasm-bindgen` browser context.
struct InMemoryModuleResolver {
    sources: HashMap<String, String>,
}

impl InMemoryModuleResolver {
    fn new(sources: HashMap<String, String>) -> Self {
        Self { sources }
    }

    fn lookup(&self, path: &str, current_dir: &str) -> Option<(String, String)> {
        // 1. Exact match — covers absolute-style ids like "main.relon".
        if let Some(src) = self.sources.get(path) {
            return Some((path.to_string(), src.clone()));
        }
        // 2. Join with current_dir for `./lib.relon` style imports. We
        //    intentionally do not `canonicalize` (no fs); a literal
        //    normalisation is good enough for the playground.
        let joined = normalise_join(current_dir, path);
        if let Some(src) = self.sources.get(&joined) {
            return Some((joined, src.clone()));
        }
        None
    }
}

impl ModuleResolver for InMemoryModuleResolver {
    fn resolve(
        &self,
        path: &str,
        scope: &Arc<Scope>,
        _range: TokenRange,
    ) -> Result<Option<ModuleSource>, RuntimeError> {
        // Std/* belongs to `StdModuleResolver`; let it through.
        if path.starts_with("std/") {
            return Ok(None);
        }
        Ok(self.lookup(path, &scope.current_dir).map(|(id, source)| {
            // `current_dir` of an in-memory module is the directory part
            // of its canonical id, so nested relative imports stay
            // anchored to the right tab.
            let current_dir = parent_dir(&id);
            ModuleSource {
                canonical_id: id,
                source,
                current_dir,
            }
        }))
    }
}

/// Lightweight path join + `./` stripping that works without `std::fs`.
/// Not a canonicaliser: it only resolves the trivial cases the
/// playground exercises (`./foo.relon`, `foo.relon`, `dir/foo.relon`).
fn normalise_join(current_dir: &str, path: &str) -> String {
    let trimmed = path.strip_prefix("./").unwrap_or(path);
    if current_dir.is_empty() || current_dir == "." {
        return trimmed.to_string();
    }
    if trimmed.starts_with('/') {
        return trimmed.to_string();
    }
    let base = current_dir.trim_end_matches('/');
    format!("{base}/{trimmed}")
}

fn parent_dir(id: &str) -> String {
    match id.rfind('/') {
        Some(idx) => id[..idx].to_string(),
        None => String::new(),
    }
}

/// Decode the `sources` argument. Accepts both shapes:
/// - `{ "main.relon": "...", "lib.relon": "..." }`
/// - `[{ "path": "main.relon", "content": "..." }, ...]`
///
/// The array form is friendlier for callers that want stable ordering
/// (e.g. a Vue `v-for` over file tabs).
fn decode_sources(value: JsValue) -> Result<HashMap<String, String>, ErrorReport> {
    let json: serde_json::Value = serde_wasm_bindgen::from_value(value).map_err(|err| {
        ErrorReport::invalid_input(format!("sources is not JSON-serialisable: {err}"))
    })?;
    match json {
        serde_json::Value::Object(map) => {
            let mut out = HashMap::with_capacity(map.len());
            for (k, v) in map {
                let s = v.as_str().ok_or_else(|| {
                    ErrorReport::invalid_input(format!(
                        "sources['{k}']: expected string content, got {}",
                        type_name(&v)
                    ))
                })?;
                out.insert(k, s.to_string());
            }
            Ok(out)
        }
        serde_json::Value::Array(items) => {
            let mut out = HashMap::with_capacity(items.len());
            for (idx, item) in items.into_iter().enumerate() {
                let obj = item.as_object().ok_or_else(|| {
                    ErrorReport::invalid_input(format!(
                        "sources[{idx}]: expected object with 'path' and 'content'"
                    ))
                })?;
                let path = obj
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ErrorReport::invalid_input(format!("sources[{idx}]: missing 'path' string"))
                    })?
                    .to_string();
                let content = obj
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ErrorReport::invalid_input(format!(
                            "sources[{idx}]: missing 'content' string"
                        ))
                    })?
                    .to_string();
                out.insert(path, content);
            }
            Ok(out)
        }
        other => Err(ErrorReport::invalid_input(format!(
            "sources: expected object or array, got {}",
            type_name(&other)
        ))),
    }
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Evaluate `entry` against an in-memory module map and return the
/// projected JSON result.
///
/// `sources` is either an object `{path: content}` or an array of
/// `{path, content}` entries. `entry` must be one of the keys.
///
/// Returns a JS value: on success the projected JSON (a plain object /
/// array / scalar); on failure a JS error whose payload is an
/// [`ErrorReport`] JSON value.
#[wasm_bindgen]
pub fn evaluate(sources: JsValue, entry: &str) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    match evaluate_internal(&sources, entry, None) {
        Ok(value) => {
            // `serde-wasm-bindgen` defaults to projecting `serde_json`
            // objects as JS `Map` instances, which surprises every JS
            // consumer (`JSON.stringify` returns `{}`, property access
            // returns `undefined`). The playground wants plain objects,
            // which is also what the README-documented contract implies
            // ("projected JSON result"). Flip the flag on the serializer
            // so maps round-trip as `{...}`.
            let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
            value.serialize(&serializer).map_err(|err| {
                err_to_js(ErrorReport::invalid_input(format!(
                    "result is not JS-serialisable: {err}"
                )))
            })
        }
        Err(report) => Err(err_to_js(report)),
    }
}

/// Evaluate `entry` as an entry program: validate `args` against the
/// file's `#main(...)` signature and bind each parameter before running
/// the body. Counterpart to the CLI's `--args` path.
///
/// `args` accepts either a JS object `{name: value}` (most common, as
/// JS callers can `JSON.parse` the user's input themselves) or `null`/
/// `undefined` for an empty map. Each value is fed through `Value`'s
/// serde deserialiser, so the JSON shape is identical to the CLI.
#[wasm_bindgen]
pub fn evaluate_main(sources: JsValue, entry: &str, args: JsValue) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let args = decode_args(args).map_err(err_to_js)?;
    match evaluate_internal(&sources, entry, Some(args)) {
        Ok(value) => {
            let serializer =
                serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
            value.serialize(&serializer).map_err(|err| {
                err_to_js(ErrorReport::invalid_input(format!(
                    "result is not JS-serialisable: {err}"
                )))
            })
        }
        Err(report) => Err(err_to_js(report)),
    }
}

fn decode_args(value: JsValue) -> Result<HashMap<String, Value>, ErrorReport> {
    if value.is_undefined() || value.is_null() {
        return Ok(HashMap::new());
    }
    let json: serde_json::Value = serde_wasm_bindgen::from_value(value).map_err(|err| {
        ErrorReport::invalid_input(format!("args is not JSON-serialisable: {err}"))
    })?;
    match json {
        serde_json::Value::Object(_) => serde_json::from_value(json).map_err(|err| {
            ErrorReport::invalid_input(format!(
                "args must be a JSON object keyed by `#main(...)` parameter names: {err}"
            ))
        }),
        other => Err(ErrorReport::invalid_input(format!(
            "args: expected object, got {}",
            type_name(&other)
        ))),
    }
}

fn evaluate_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    args: Option<HashMap<String, Value>>,
) -> Result<serde_json::Value, ErrorReport> {
    let source = sources.get(entry).ok_or_else(|| {
        ErrorReport::invalid_input(format!(
            "entry '{entry}' not found in sources (have {} files)",
            sources.len()
        ))
    })?;

    // Stage 0: entry-level parse pre-check. Mirrors the facade's
    // behaviour of surfacing entry parse failures distinctly from the
    // workspace's `ModuleParseError` (which is shaped for imported
    // modules).
    if let Err(err) = parse_document(source) {
        return Err(ErrorReport::from_parse(err.to_string()));
    }

    // Build the resolver chain: in-memory first, std fallback. No
    // filesystem resolver — that's the sandbox boundary the wasm
    // playground enforces.
    let in_memory: Arc<dyn ModuleResolver> = Arc::new(InMemoryModuleResolver::new(sources.clone()));
    let std_resolver: Arc<dyn ModuleResolver> = Arc::new(StdModuleResolver);

    let entry_dir = parent_dir(entry);
    let entry_dir_path = PathBuf::from(if entry_dir.is_empty() {
        ".".to_string()
    } else {
        entry_dir.clone()
    });

    // Drive the analyzer in workspace mode against the same resolver
    // chain we will use at eval time. Custom posture: explicit
    // resolver vec, no filesystem fallback.
    let mut loader = ResolverChainLoader::from_resolvers(vec![
        Arc::clone(&in_memory),
        Arc::clone(&std_resolver),
    ]);
    let workspace = analyze_entry(entry.to_string(), source, entry_dir_path, &mut loader);

    if workspace.has_errors() {
        return Err(workspace_to_report(&workspace));
    }

    let entry_node = workspace
        .nodes
        .get(entry)
        .map(|arc| (**arc).clone())
        .unwrap_or_else(|| {
            parse_document(source).expect("workspace passed but entry no longer parseable")
        });

    let workspace_arc = Arc::new(workspace);

    // Sandboxed `Context` + in-memory resolver. Capabilities default
    // to "all denied" via `Context::sandboxed()` — we never flip them
    // on, so any host-fn call that touches a real capability fails
    // cleanly with `CapabilityDenied` (visible in the UI as an
    // `EvalError`). That's the demo-correct behaviour.
    let mut ctx = Context::sandboxed()
        .with_root(entry_node)
        .with_workspace(Arc::clone(&workspace_arc));
    ctx.prepend_module_resolver(in_memory);
    let ctx = Arc::new(ctx);

    let _root_loading_guard = ctx.enter_loading_module(entry.to_string());

    let evaluator = Evaluator::new(Arc::clone(&ctx));

    let mut root_scope = Scope::default();
    root_scope.current_dir = entry_dir;
    root_scope.cache_namespace = entry.to_string();

    let scope_arc = Arc::new(root_scope);
    let value = match args {
        Some(args_map) => evaluator
            .run_main(&scope_arc, args_map)
            .map_err(|err| ErrorReport::from_miette(ErrorKind::EvalError, &err, Some(entry)))?,
        None => evaluator.eval_root(&scope_arc).map_err(|err| {
            // The browser sandbox's auto-evaluate runs `eval_root`, not
            // `run_main`, so a `#main(Order order)` script reaches
            // `order.items` with no binding for `order` and the
            // generic `VariableNotFound` surfaces. That's technically
            // accurate but misleading — `order` is right there in the
            // signature — so we rewrite it to `MissingMainArg`, which
            // says what's actually missing. Anything else passes
            // through untouched.
            let err = rewrite_missing_main_arg(&ctx, err);
            ErrorReport::from_miette(ErrorKind::EvalError, &err, Some(entry))
        })?,
    };

    relon::to_json_value(value).map_err(|err| match err {
        relon::Error::NonFiniteFloat(_)
        | relon::Error::UnsupportedClosure
        | relon::Error::UnsupportedSchema => ErrorReport {
            kind: ErrorKind::ProjectionError,
            message: err.to_string(),
            spans: Vec::new(),
            help: None,
            code: None,
        },
        other => ErrorReport {
            kind: ErrorKind::ProjectionError,
            message: other.to_string(),
            spans: Vec::new(),
            help: None,
            code: None,
        },
    })
}

/// Reframe a generic `VariableNotFound` as `MissingMainArg` when the
/// missing name is one declared by the entry's `#main(...)` signature.
///
/// We run scripts via `eval_root` rather than `run_main`, so a
/// `#main(Order order)` script that references `order.items` raises
/// `VariableNotFound("order")`. The playground user sees `order` right
/// there in the signature and reasonably finds that error misleading.
/// `MissingMainArg` says what's actually wrong and points at the
/// missing argument — same diagnostic the evaluator emits when a host
/// calls `run_main` with an incomplete arg map.
fn rewrite_missing_main_arg(ctx: &Context, err: RuntimeError) -> RuntimeError {
    let name = match &err {
        RuntimeError::VariableNotFound(n, _) => n.clone(),
        _ => return err,
    };
    // Point the marker at the parameter's declaration site (`#main(...)
    // <name>`) — that's the contract the missing arg violates. The
    // use-site is just where evaluation noticed; matching `run_main`'s
    // own behaviour keeps both paths consistent.
    let param_range = ctx
        .analyzed
        .as_ref()
        .and_then(|tree| tree.main_signature.as_ref())
        .and_then(|sig| sig.params.iter().find(|p| p.name == name))
        .map(|p| p.range);
    match param_range {
        Some(range) => RuntimeError::MissingMainArg { name, range },
        None => err,
    }
}

fn workspace_to_report(workspace: &relon_analyzer::WorkspaceTree) -> ErrorReport {
    // Collect workspace-level error diagnostics first, then per-module
    // errors. Each contributes its own `SpanInfo`s tagged with the
    // owning module (or `None` for workspace-level entries).
    let mut spans = Vec::new();
    let mut messages: Vec<String> = Vec::new();
    let mut help: Option<String> = None;
    let mut code: Option<String> = None;

    for diag in &workspace.workspace_diagnostics {
        if diag.severity() != Severity::Error {
            continue;
        }
        messages.push(diag.to_string());
        if help.is_none() {
            help = diag.help().map(|c| c.to_string());
        }
        if code.is_none() {
            code = diag.code().map(|c| c.to_string());
        }
        if let Some(labels) = diag.labels() {
            for label in labels {
                let span = label.inner();
                spans.push(SpanInfo {
                    file: None,
                    start: span.offset(),
                    end: span.offset() + span.len(),
                    label: label.label().map(|s| s.to_string()),
                });
            }
        }
    }

    for (module_id, tree) in &workspace.modules {
        for diag in &tree.diagnostics {
            if diag.severity() != Severity::Error {
                continue;
            }
            messages.push(format!("[{module_id}] {diag}"));
            if help.is_none() {
                help = diag.help().map(|c| c.to_string());
            }
            if code.is_none() {
                code = diag.code().map(|c| c.to_string());
            }
            if let Some(labels) = diag.labels() {
                for label in labels {
                    let span = label.inner();
                    spans.push(SpanInfo {
                        file: Some(module_id.clone()),
                        start: span.offset(),
                        end: span.offset() + span.len(),
                        label: label.label().map(|s| s.to_string()),
                    });
                }
            }
        }
    }

    ErrorReport {
        kind: ErrorKind::AnalyzeError,
        message: if messages.is_empty() {
            "analyzer reported errors".to_string()
        } else {
            messages.join("; ")
        },
        spans,
        help,
        code,
    }
}

/// Pretty-print a Relon source string using `relon-fmt`. Returns the
/// formatted source on success, or an [`ErrorReport`] payload on
/// failure (parse error or formatter check failure).
#[wasm_bindgen]
pub fn format(content: &str) -> Result<String, JsValue> {
    relon_fmt::format_source(content).map_err(|err| {
        err_to_js(ErrorReport {
            kind: ErrorKind::ParseError,
            message: err.to_string(),
            spans: Vec::new(),
            help: None,
            code: None,
        })
    })
}

/// Crate version, exposed for UI footers / cache busters. Sourced from
/// `CARGO_PKG_VERSION`; tracks the workspace version.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Result of a successful go-to-definition lookup. Sent to JS as a
/// plain object — fields are flat so consumers can dispatch a
/// CodeMirror selection without parsing nested shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GotoDefinitionResult {
    /// Path of the target file inside the `sources` map. Same string
    /// the entry was identified by, or a different module's canonical
    /// id reached through `#import`.
    pub path: String,
    /// Start `(line, character)` of the target range. `character` is
    /// a UTF-16 code-unit index (matches `CodeMirror`'s + LSP's
    /// position convention).
    pub start: Position,
    /// End `(line, character)` of the target range. For "jump to top
    /// of file" cases (cursor on a `#import` path string or on an
    /// alias head alone), `start == end == (0, 0)`.
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Resolve a cursor position to its definition. Mirrors what the LSP's
/// `textDocument/definition` handler does, but driven entirely from
/// the in-memory `sources` map so the browser playground gets the
/// same semantics without a filesystem.
///
/// `sources` is the same object/array shape as [`evaluate`]: either
/// `{ path: content }` or `[ { path, content } ]`. `entry` is one of
/// the keys (the file the cursor sits in). `line` / `character` are
/// 0-based, with `character` measured in UTF-16 code units
/// (CodeMirror / LSP convention).
///
/// Returns `null` (JS) when the cursor isn't on a recognisable
/// reference, when the target couldn't be located in the workspace,
/// or when the workspace failed to build (parse errors etc. — the
/// caller can re-run `evaluate` to get a structured error report).
#[wasm_bindgen]
pub fn goto_definition(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let target = match goto_definition_internal(&sources, entry, line, character) {
        Some(t) => t,
        None => return Ok(JsValue::NULL),
    };
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    target.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "goto_definition result is not JS-serialisable: {err}"
        )))
    })
}

fn goto_definition_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
) -> Option<GotoDefinitionResult> {
    use relon_analyzer::goto_def::{self, GotoTarget};

    let source = sources.get(entry)?;

    // Re-use the same loader chain that `evaluate_internal` uses —
    // in-memory first, std fallback — so `#import` resolution lands on
    // the playground's tabs and not the filesystem.
    let in_memory: Arc<dyn ModuleResolver> =
        Arc::new(InMemoryModuleResolver::new(sources.clone()));
    let std_resolver: Arc<dyn ModuleResolver> = Arc::new(StdModuleResolver);
    let entry_dir = parent_dir(entry);
    let entry_dir_path = PathBuf::from(if entry_dir.is_empty() {
        ".".to_string()
    } else {
        entry_dir.clone()
    });
    let mut loader = ResolverChainLoader::from_resolvers(vec![in_memory, std_resolver]);
    let workspace = relon_analyzer::workspace::analyze_entry(
        entry.to_string(),
        source,
        entry_dir_path,
        &mut loader,
    );

    // Even if the workspace reports errors we still try to resolve —
    // the cross-module map only gets populated on successful post-pass,
    // but same-file `references` survive most analyzer errors. A
    // user editing through a transient parse error shouldn't lose
    // navigation.
    let entry_tree = workspace.modules.get(entry)?;
    let entry_root = workspace.nodes.get(entry)?;

    let target = goto_def::resolve(
        source,
        entry_root,
        entry_tree,
        Some(&workspace),
        Some(entry),
        line,
        character,
    )?;

    match target {
        GotoTarget::Node {
            module_id,
            start,
            end,
        } => {
            let target_path = module_id.unwrap_or_else(|| entry.to_string());
            let target_source = sources.get(&target_path)?;
            let (s_line, s_char) = goto_def::offset_to_position(target_source, start);
            let (e_line, e_char) = goto_def::offset_to_position(target_source, end);
            Some(GotoDefinitionResult {
                path: target_path,
                start: Position {
                    line: s_line,
                    character: s_char,
                },
                end: Position {
                    line: e_line,
                    character: e_char,
                },
            })
        }
        GotoTarget::ImportPath {
            raw_path,
            canonical_id,
        } => {
            // Prefer the workspace-resolved canonical id; fall back to
            // the raw path if the import didn't resolve (the file
            // doesn't exist in `sources` — the playground UI surfaces
            // that as a module-not-found diagnostic elsewhere).
            let target_path = canonical_id.unwrap_or(raw_path);
            // Only honour paths that actually exist in the sources
            // map. `std/...` and non-existent files return null so
            // the UI doesn't try to switch to a tab that isn't there.
            if !sources.contains_key(&target_path) {
                return None;
            }
            Some(GotoDefinitionResult {
                path: target_path,
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            })
        }
    }
}

/// One completion candidate. Sent to JS as a plain object so the
/// CodeMirror callback can map to `Completion` without parsing
/// nested shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResult {
    pub label: String,
    /// One of: "method", "field", "param", "schema", "stdlib",
    /// "module", "import", "reference", "directive", "pragma",
    /// "decorator", "keyword".
    pub kind: String,
    /// Right-aligned label shown after the suggestion (e.g. file
    /// path of an import). Optional.
    pub detail: Option<String>,
}

/// Resolve a cursor position to a list of completion candidates.
/// Mirrors the LSP's `textDocument/completion` handler, sharing the
/// analyzer's [`relon_analyzer::complete::resolve`] under the hood.
///
/// Same `sources` / `entry` / `line` / `character` semantics as
/// [`goto_definition`]. Returns an array of `CompletionResult`
/// objects (possibly empty); never `null`. Parse errors in the entry
/// source surface as an empty array so the editor doesn't break
/// completion while the user is mid-edit.
#[wasm_bindgen]
pub fn complete(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let results = complete_internal(&sources, entry, line, character).unwrap_or_default();
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    results.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "complete result is not JS-serialisable: {err}"
        )))
    })
}

fn complete_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
) -> Option<Vec<CompletionResult>> {
    use relon_analyzer::complete;

    let source = sources.get(entry)?;

    // Try the full scope-aware path on the unmodified source first
    // — that's the common case (cursor on whitespace, inside a
    // syntactically complete construct).
    if let Some(items) = try_full_complete(sources, entry, source, line, character) {
        return Some(items.into_iter().map(into_result).collect());
    }

    // Recovery path. Mid-edit input is broken in some way: typically a
    // half-typed identifier (`lib.fo│`) or a trigger char waiting for
    // its target (`&│`, `@│`, `lib.│`). We can't fix the parser, so
    // we patch the broken span with an equally-long numeric literal
    // (always a valid expression), re-parse, and walk the recovered
    // AST for scope. Cursor-context classification still runs against
    // the *original* bytes so we know whether to offer references,
    // decorators, member candidates, etc.
    let offset = relon_analyzer::goto_def::position_to_offset(source, line, character);
    if let Some(patched) = patch_in_progress_token(source, offset) {
        let mut patched_sources = sources.clone();
        patched_sources.insert(entry.to_string(), patched.clone());
        if let Some(items) = try_full_complete(&patched_sources, entry, source, line, character)
        {
            return Some(items.into_iter().map(into_result).collect());
        }
    }

    // Final fallback: pure source-byte cursor classification. Always
    // returns *something* for `#` / `@` / `&` triggers even when the
    // file is severely broken; empty list for bare contexts that need
    // an AST.
    let items = complete::keywords_for_cursor(source, line, character);
    Some(items.into_iter().map(into_result).collect())
}

/// Run the scope-aware completion against `sources`. `display_source`
/// is the bytes used for cursor-context classification — pass the
/// user's actual document, not the patched one, so the classifier
/// sees the prefix character the user typed.
fn try_full_complete(
    sources: &HashMap<String, String>,
    entry: &str,
    display_source: &str,
    line: u32,
    character: u32,
) -> Option<Vec<relon_analyzer::complete::CompletionItem>> {
    use relon_analyzer::complete;

    let parse_source = sources.get(entry)?;
    let in_memory: Arc<dyn ModuleResolver> =
        Arc::new(InMemoryModuleResolver::new(sources.clone()));
    let std_resolver: Arc<dyn ModuleResolver> = Arc::new(StdModuleResolver);
    let entry_dir = parent_dir(entry);
    let entry_dir_path = PathBuf::from(if entry_dir.is_empty() {
        ".".to_string()
    } else {
        entry_dir.clone()
    });
    let mut loader = ResolverChainLoader::from_resolvers(vec![in_memory, std_resolver]);
    let workspace = relon_analyzer::workspace::analyze_entry(
        entry.to_string(),
        parse_source,
        entry_dir_path,
        &mut loader,
    );
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    Some(complete::resolve(
        display_source,
        root,
        tree,
        Some(&workspace),
        line,
        character,
    ))
}

/// Replace the user's in-progress token at `offset` with an equally-
/// long numeric placeholder so the rest of the file can parse. The
/// "token" is grabbed by walking back from the cursor over identifier
/// chars, then optionally over one `.<ident>` segment (so `lib.fo│`
/// is replaced as a unit), then optionally over the leading
/// `#`/`@`/`&` trigger char (so the prefix-only `#│` / `&│` case
/// turns into a valid `1`).
///
/// Returns `None` when there's nothing meaningful to patch (cursor in
/// whitespace, at file start, or the token doesn't fit a numeric
/// placeholder).
fn patch_in_progress_token(source: &str, offset: usize) -> Option<String> {
    let bytes = source.as_bytes();
    let pos = offset.min(bytes.len());

    let mut start = pos;
    // Walk back over identifier chars.
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    // For member-style `head.tail` access, swallow the dot + head.
    if start > 0 && bytes[start - 1] == b'.' {
        start -= 1;
        while start > 0 && is_ident_byte(bytes[start - 1]) {
            start -= 1;
        }
    }
    // Swallow the trigger character itself (`#` / `@` / `&`) so a
    // bare `#│` doesn't leave behind an orphan `#` that the parser
    // would still reject.
    if start > 0 && matches!(bytes[start - 1], b'#' | b'@' | b'&') {
        start -= 1;
    }

    let span_len = pos.checked_sub(start)?;
    if span_len == 0 {
        return None;
    }

    // Numeric literal of matching length. `1` is always a valid
    // expression; repeating it produces a valid larger integer.
    let placeholder: String = std::iter::repeat('1').take(span_len).collect();
    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..start]);
    out.push_str(&placeholder);
    out.push_str(&source[pos..]);
    Some(out)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn into_result(item: relon_analyzer::complete::CompletionItem) -> CompletionResult {
    use relon_analyzer::complete::CompletionKind;
    CompletionResult {
        label: item.label,
        kind: match item.kind {
            CompletionKind::Method => "method",
            CompletionKind::Field => "field",
            CompletionKind::Parameter => "param",
            CompletionKind::Schema => "schema",
            CompletionKind::Stdlib => "stdlib",
            CompletionKind::Module => "module",
            CompletionKind::Import => "import",
            CompletionKind::Reference => "reference",
            CompletionKind::Directive => "directive",
            CompletionKind::Pragma => "pragma",
            CompletionKind::Decorator => "decorator",
            CompletionKind::Keyword => "keyword",
        }
        .to_string(),
        detail: item.detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_file(content: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("main.relon".to_string(), content.to_string());
        m
    }

    #[test]
    fn evaluates_single_file_arithmetic() {
        let sources = single_file(r#"{ val: 1 + 2 * 3 }"#);
        let value = evaluate_internal(&sources, "main.relon", None).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "val": 7
            })
        );
    }

    #[test]
    fn evaluates_two_file_import() {
        // Two-file scenario: main imports lib, uses its exported value.
        // Mirrors the typical playground multi-tab flow.
        let mut sources = HashMap::new();
        sources.insert(
            "main.relon".to_string(),
            r#"#import lib from "./lib.relon"
{
    greeting: lib.hello + ", world"
}"#
            .to_string(),
        );
        sources.insert("lib.relon".to_string(), r#"{ hello: "hi" }"#.to_string());
        let value = evaluate_internal(&sources, "main.relon", None).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "greeting": "hi, world"
            })
        );
    }

    #[test]
    fn parse_error_surfaces_as_parse_kind() {
        let sources = single_file("{ not closed");
        let err = evaluate_internal(&sources, "main.relon", None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn missing_entry_is_invalid_input() {
        let sources = single_file("{ a: 1 }");
        let err = evaluate_internal(&sources, "does-not-exist.relon", None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn fs_import_denied_in_sandbox() {
        // The wasm playground never mounts a FilesystemModuleResolver,
        // so a stray relative import that isn't in the in-memory map
        // must fail at workspace-analysis time with AnalyzeError
        // (ModuleParseError / module not found), not silently fall
        // through to disk. Concrete behaviour: workspace pass surfaces
        // a workspace-level error.
        let sources = single_file(
            r#"#import missing from "./missing.relon"
{
    x: missing.value
}"#,
        );
        let err = evaluate_internal(&sources, "main.relon", None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::AnalyzeError);
    }

    #[test]
    fn format_passes_through_relon_fmt() {
        // Smoke test: format() is just a wrapper, so we only check it
        // doesn't reject a valid program and returns *some* output.
        let out = relon_fmt::format_source("{a:1,b:2}").unwrap();
        assert!(out.contains('a'));
        assert!(out.contains('b'));
    }

    #[test]
    fn version_matches_cargo_pkg_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
