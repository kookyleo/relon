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
// `relon::ResolverChainLoader` + the facade-level `RuntimeError` /
// `Scope` / `Value` cover the public surface. Lower-level
// `Context` / `TreeWalkEvaluator` and the custom module-resolver
// types stay direct reach: the playground installs an
// in-memory `ModuleResolver` chain and threads it through
// `Context::sandboxed()` + `TreeWalkEvaluator::prepare_in_place`,
// which the facade deliberately doesn't expose.
use relon::{ResolverChainLoader, RuntimeError, Scope, Value};
use relon_analyzer::{analyze_entry, Severity};
use relon_evaluator::module::{ModuleResolver, ModuleSource, StdModuleResolver};
use relon_evaluator::{Context, TreeWalkEvaluator};
use relon_parser::{parse_document, parse_document_recovering, TokenRange};
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

/// The crate-wide `serde_wasm_bindgen` serializer config. Every
/// wasm-bindgen entry that returns a Rust struct to JS must route
/// through this helper so the `Vec<HashMap<_,_>>` shaped payloads
/// (`spans`, `diagnostics`, etc.) deserialize on the JS side as
/// JS objects rather than `Map` instances — without the
/// `serialize_maps_as_objects(true)` flag downstream `err.kind` /
/// `diag.range` field reads would silently fail.
fn js_serializer() -> serde_wasm_bindgen::Serializer {
    serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true)
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
    let serializer = js_serializer();
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
/// `{path, content}` entries. `entry` must be one of the keys. `args`
/// is optional — pass `null`, `undefined`, omit the parameter, or pass
/// a **JSON string** keyed by `#main(...)` parameter names.
///
/// The JSON-string shape (rather than a JS object) is deliberate: JS
/// has only one Number type, so `JSON.parse("100.0")` collapses to the
/// same value as `JSON.parse("100")`, and the wasm boundary loses the
/// Int vs Float distinction the `#main(...)` signature relies on.
/// Parsing on the Rust side with `serde_json::from_str` preserves the
/// distinction (`100` → `Int`, `100.0` → `Float`).
///
/// One entry covers both no-args scripts (root-expression evaluation)
/// and `#main(...)` entry programs — the script's declaration is what
/// decides which path runs, not the caller. A script that declares
/// `#main(...)` and receives no args (or args missing a parameter)
/// still surfaces `relon::eval::missing_main_arg` as the live teaching
/// signal it always did.
///
/// Returns a JS value: on success the projected JSON (a plain object /
/// array / scalar); on failure a JS error whose payload is an
/// [`ErrorReport`] JSON value.
#[wasm_bindgen]
pub fn evaluate(sources: JsValue, entry: &str, args: JsValue) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let args_opt = decode_args_json(args).map_err(err_to_js)?;
    match evaluate_internal(&sources, entry, args_opt) {
        Ok(value) => {
            // `serde-wasm-bindgen` defaults to projecting `serde_json`
            // objects as JS `Map` instances, which surprises every JS
            // consumer (`JSON.stringify` returns `{}`, property access
            // returns `undefined`). The playground wants plain objects,
            // which is also what the README-documented contract implies
            // ("projected JSON result"). Flip the flag on the serializer
            // so maps round-trip as `{...}`.
            let serializer = js_serializer();
            value.serialize(&serializer).map_err(|err| {
                err_to_js(ErrorReport::invalid_input(format!(
                    "result is not JS-serialisable: {err}"
                )))
            })
        }
        Err(report) => Err(err_to_js(report)),
    }
}

/// Decode the `args` parameter into an optional `HashMap`. Accepts:
///
///   - `null` / `undefined` / missing → `None` (no caller-supplied args)
///   - JS string → parsed with `serde_json::from_str`, which preserves
///     the JSON number's int-vs-float shape (`"100"` → `Int(100)`,
///     `"100.0"` → `Float(100.0)`). This is the lossless path the
///     playground uses to round-trip preset `defaultArgs` through to
///     the evaluator without JS Number collapsing `.0`.
///   - Anything else → `InvalidInput`. We intentionally don't accept a
///     JS object here, because object-shaped input would have to pass
///     through `serde_wasm_bindgen::from_value` and lose the Int/Float
///     distinction at the boundary.
fn decode_args_json(value: JsValue) -> Result<Option<HashMap<String, Value>>, ErrorReport> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    let text = value.as_string().ok_or_else(|| {
        ErrorReport::invalid_input(
            "args must be a JSON string (e.g. `JSON.stringify({...})`) so int/float distinction \
             isn't lost at the wasm boundary; pass `null` or omit for no args"
                .to_string(),
        )
    })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let json: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|err| ErrorReport::invalid_input(format!("args is not valid JSON: {err}")))?;
    match json {
        serde_json::Value::Object(_) => {
            let map = serde_json::from_value(json).map_err(|err| {
                ErrorReport::invalid_input(format!(
                    "args must be a JSON object keyed by `#main(...)` parameter names: {err}"
                ))
            })?;
            Ok(Some(map))
        }
        other => Err(ErrorReport::invalid_input(format!(
            "args: expected JSON object, got {}",
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
    let ctx = Arc::new({
        let mut ctx = ctx;
        relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });

    let _root_loading_guard = ctx.enter_loading_module(entry.to_string());

    let evaluator = TreeWalkEvaluator::new(Arc::clone(&ctx));

    let scope_arc = Arc::new(Scope {
        current_dir: entry_dir,
        cache_namespace: entry.to_string(),
        ..Scope::default()
    });
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
    let serializer = js_serializer();
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
    let in_memory: Arc<dyn ModuleResolver> = Arc::new(InMemoryModuleResolver::new(sources.clone()));
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

/// Cursor-position hover info. `markdown` is the rendered tooltip
/// body; `range_*_offset` are byte offsets into the entry source so
/// the caller can position the tooltip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoverResult {
    pub markdown: String,
    pub range_start_offset: u32,
    pub range_end_offset: u32,
}

/// Cursor-position signature help. `signature` is a rendered string
/// like `currency(val: String, symbol: String) -> String`;
/// `active_parameter` indexes which slot the cursor sits in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureHelpResult {
    pub signature: String,
    pub active_parameter: u32,
    pub range_start_offset: u32,
    pub range_end_offset: u32,
}

/// Resolve a hover request at `(line, character)`. Returns the
/// rendered tooltip + the source range it describes, or `null` when
/// the cursor isn't on a hoverable symbol.
#[wasm_bindgen]
pub fn hover(sources: JsValue, entry: &str, line: u32, character: u32) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result = match hover_internal(&sources, entry, line, character) {
        Some(r) => r,
        None => return Ok(JsValue::NULL),
    };
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "hover result is not JS-serialisable: {err}"
        )))
    })
}

fn hover_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
) -> Option<HoverResult> {
    let source = sources.get(entry)?;
    let workspace = build_workspace(sources, entry, source);
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    let info = relon_analyzer::hover::resolve(source, root, tree, line, character)?;
    Some(HoverResult {
        markdown: info.markdown,
        range_start_offset: info.range.start.offset as u32,
        range_end_offset: info.range.end.offset as u32,
    })
}

/// Resolve a signature-help request. Returns the rendered callee
/// signature + the active parameter index, or `null` when the cursor
/// isn't inside a function call's argument list.
#[wasm_bindgen]
pub fn signature_help(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result = match signature_help_internal(&sources, entry, line, character) {
        Some(r) => r,
        None => return Ok(JsValue::NULL),
    };
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "signature_help result is not JS-serialisable: {err}"
        )))
    })
}

fn signature_help_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
) -> Option<SignatureHelpResult> {
    let source = sources.get(entry)?;
    let workspace = build_workspace(sources, entry, source);
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    let info = relon_analyzer::signature_help::resolve(source, root, tree, line, character)?;
    Some(SignatureHelpResult {
        signature: info.signature,
        active_parameter: info.active_parameter as u32,
        range_start_offset: info.range.start.offset as u32,
        range_end_offset: info.range.end.offset as u32,
    })
}

/// One quick-fix candidate from the analyzer. `edits` reuses the
/// rename text-edit shape so the JS side has one apply path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeActionWire {
    pub title: String,
    pub diagnostic_code: Option<String>,
    pub edits: Vec<TextEditWire>,
}

/// Collect every quick-fix at `(line, character)`. Returns an empty
/// array when no diagnostic anchors there or none of the anchored
/// diagnostics have an automated fix today.
#[wasm_bindgen]
pub fn code_actions(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result = code_actions_internal(&sources, entry, line, character).unwrap_or_default();
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "code_actions result is not JS-serialisable: {err}"
        )))
    })
}

fn code_actions_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
) -> Option<Vec<CodeActionWire>> {
    let source = sources.get(entry)?;
    let workspace = build_workspace(sources, entry, source);
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    Some(
        relon_analyzer::code_actions::at_position(source, root, tree, line, character)
            .into_iter()
            .map(|a| CodeActionWire {
                title: a.title,
                diagnostic_code: a.diagnostic_code,
                edits: a
                    .edits
                    .into_iter()
                    .map(|e| TextEditWire {
                        start_line: e.range.start.line,
                        start_character: e.range.start.column as u32,
                        end_line: e.range.end.line,
                        end_character: e.range.end.column as u32,
                        start_offset: e.range.start.offset as u32,
                        end_offset: e.range.end.offset as u32,
                        new_text: e.new_text,
                    })
                    .collect(),
            })
            .collect(),
    )
}

/// One outline entry returned by `document_symbols`. `parent` is an
/// index into the same vector — `None` for top-level entries — so the
/// caller can rebuild the tree without re-walking source. `kind` is a
/// short string the IDE maps to its own icon set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbolWire {
    pub name: String,
    pub kind: String,
    pub parent: Option<u32>,
    pub doc: Option<String>,
    pub range_start_line: u32,
    pub range_start_character: u32,
    pub range_end_line: u32,
    pub range_end_character: u32,
    pub range_start_offset: u32,
    pub range_end_offset: u32,
    pub selection_start_line: u32,
    pub selection_start_character: u32,
    pub selection_end_line: u32,
    pub selection_end_character: u32,
    pub selection_start_offset: u32,
    pub selection_end_offset: u32,
}

/// Collect every outline-relevant symbol declared in the entry. Cheap
/// to call on every keystroke; runs a single AST walk.
#[wasm_bindgen]
pub fn document_symbols(sources: JsValue, entry: &str) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result = document_symbols_internal(&sources, entry).unwrap_or_default();
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "document_symbols result is not JS-serialisable: {err}"
        )))
    })
}

fn document_symbols_internal(
    sources: &HashMap<String, String>,
    entry: &str,
) -> Option<Vec<DocumentSymbolWire>> {
    let source = sources.get(entry)?;
    let workspace = build_workspace(sources, entry, source);
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    Some(
        relon_analyzer::symbols::collect(root, tree)
            .into_iter()
            .map(|s| DocumentSymbolWire {
                name: s.name,
                kind: match s.kind {
                    relon_analyzer::symbols::SymbolKind::Schema => "schema".into(),
                    relon_analyzer::symbols::SymbolKind::Method => "method".into(),
                    relon_analyzer::symbols::SymbolKind::Field => "field".into(),
                    relon_analyzer::symbols::SymbolKind::SchemaField => "schema-field".into(),
                    relon_analyzer::symbols::SymbolKind::Import => "import".into(),
                },
                parent: s.parent.map(|p| p as u32),
                doc: s.doc,
                range_start_line: s.range.start.line,
                range_start_character: s.range.start.column as u32,
                range_end_line: s.range.end.line,
                range_end_character: s.range.end.column as u32,
                range_start_offset: s.range.start.offset as u32,
                range_end_offset: s.range.end.offset as u32,
                selection_start_line: s.selection_range.start.line,
                selection_start_character: s.selection_range.start.column as u32,
                selection_end_line: s.selection_range.end.line,
                selection_end_character: s.selection_range.end.column as u32,
                selection_start_offset: s.selection_range.start.offset as u32,
                selection_end_offset: s.selection_range.end.offset as u32,
            })
            .collect(),
    )
}

/// One text replacement returned by `rename`. Coordinates are
/// LSP-style (0-indexed line, UTF-16 character) plus the equivalent
/// byte offsets so a browser caller can pick whichever it prefers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEditWire {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub new_text: String,
}

/// Result of `prepare_rename`. `valid: false` means the cursor isn't
/// on a renamable symbol; `error` carries the human-readable reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRenameResult {
    pub valid: bool,
    pub error: Option<String>,
    pub placeholder: Option<String>,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub start_offset: u32,
    pub end_offset: u32,
}

/// Probe whether the cursor is on a renamable symbol and, if so, return
/// the range whose text would be replaced. The playground uses this to
/// seed an inline rename input — failing fast gives clearer UX than a
/// silent no-op.
#[wasm_bindgen]
pub fn prepare_rename(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result = prepare_rename_internal(&sources, entry, line, character);
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "prepare_rename result is not JS-serialisable: {err}"
        )))
    })
}

fn prepare_rename_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
) -> PrepareRenameResult {
    let invalid = |msg: String| PrepareRenameResult {
        valid: false,
        error: Some(msg),
        placeholder: None,
        start_line: 0,
        start_character: 0,
        end_line: 0,
        end_character: 0,
        start_offset: 0,
        end_offset: 0,
    };
    let Some(source) = sources.get(entry) else {
        return invalid(format!("entry `{entry}` not found in sources"));
    };
    let workspace = build_workspace(sources, entry, source);
    let (Some(tree), Some(root)) = (workspace.modules.get(entry), workspace.nodes.get(entry))
    else {
        return invalid(format!("entry `{entry}` did not analyse"));
    };
    match relon_analyzer::rename::prepare(source, root, tree, line, character) {
        Ok(range) => {
            let placeholder = source
                .get(range.start.offset..range.end.offset)
                .map(|s| s.to_string());
            PrepareRenameResult {
                valid: true,
                error: None,
                placeholder,
                start_line: range.start.line,
                start_character: range.start.column as u32,
                end_line: range.end.line,
                end_character: range.end.column as u32,
                start_offset: range.start.offset as u32,
                end_offset: range.end.offset as u32,
            }
        }
        Err(err) => invalid(format!("{err:?}")),
    }
}

/// Compute the edit list for renaming the symbol at `(line, character)`
/// to `new_name`. Returns a structured `ErrorReport` (kind = InvalidInput)
/// on failure so the playground can surface the reason in a toast.
#[wasm_bindgen]
pub fn rename_symbol(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
    new_name: &str,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let source = sources
        .get(entry)
        .ok_or_else(|| {
            err_to_js(ErrorReport::invalid_input(format!(
                "entry `{entry}` not found in sources"
            )))
        })?
        .clone();
    let workspace = build_workspace(&sources, entry, &source);
    let tree = workspace.modules.get(entry).ok_or_else(|| {
        err_to_js(ErrorReport::invalid_input(format!(
            "entry `{entry}` did not analyse"
        )))
    })?;
    let root = workspace.nodes.get(entry).ok_or_else(|| {
        err_to_js(ErrorReport::invalid_input(format!(
            "entry `{entry}` did not analyse"
        )))
    })?;
    let edits = relon_analyzer::rename::execute(&source, root, tree, line, character, new_name)
        .map_err(|err| err_to_js(ErrorReport::invalid_input(format!("{err:?}"))))?;
    let wire: Vec<TextEditWire> = edits
        .into_iter()
        .map(|e| TextEditWire {
            start_line: e.range.start.line,
            start_character: e.range.start.column as u32,
            end_line: e.range.end.line,
            end_character: e.range.end.column as u32,
            start_offset: e.range.start.offset as u32,
            end_offset: e.range.end.offset as u32,
            new_text: e.new_text,
        })
        .collect();
    let serializer = js_serializer();
    wire.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "rename result is not JS-serialisable: {err}"
        )))
    })
}

/// One inlay-hint to render in the editor gutter / inline.
/// `position_*` mark where the ghost text should sit; the CodeMirror
/// playground passes them straight into a `Decoration.widget`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlayHintWire {
    pub line: u32,
    pub character: u32,
    pub offset: u32,
    pub label: String,
    pub kind: String,
}

/// Collect every inlay hint the analyzer can derive for `entry`.
/// Cheap to run on every keystroke for typical-sized modules: a single
/// AST walk + signature lookups. Returns an empty array when the entry
/// can't be parsed (the playground keeps the previous hints visible
/// rather than thrashing).
#[wasm_bindgen]
pub fn inlay_hints(sources: JsValue, entry: &str) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result = inlay_hints_internal(&sources, entry).unwrap_or_default();
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "inlay_hints result is not JS-serialisable: {err}"
        )))
    })
}

fn inlay_hints_internal(
    sources: &HashMap<String, String>,
    entry: &str,
) -> Option<Vec<InlayHintWire>> {
    let source = sources.get(entry)?;
    let workspace = build_workspace(sources, entry, source);
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    Some(
        relon_analyzer::inlay_hints::collect(root, tree)
            .into_iter()
            .map(|h| InlayHintWire {
                line: h.line,
                character: h.character,
                offset: h.offset as u32,
                label: h.label,
                kind: match h.kind {
                    relon_analyzer::inlay_hints::InlayHintKind::Parameter => "parameter".into(),
                },
            })
            .collect(),
    )
}

/// A single find-references hit. `start`/`end` mirror the LSP-style
/// `Position` shape so the browser caller can highlight or jump
/// without re-walking the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub start_offset: u32,
    pub end_offset: u32,
}

/// Resolve find-references at `(line, character)`. When the cursor
/// sits on a reference site or a dict-field declaration, returns every
/// in-file occurrence (plus the declaration when `include_declaration`
/// is true). Returns `null` when the cursor isn't over a recognised
/// symbol, and an empty array when the symbol has no references.
#[wasm_bindgen]
pub fn find_references(
    sources: JsValue,
    entry: &str,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Result<JsValue, JsValue> {
    let sources = decode_sources(sources).map_err(err_to_js)?;
    let result =
        match find_references_internal(&sources, entry, line, character, include_declaration) {
            Some(r) => r,
            None => return Ok(JsValue::NULL),
        };
    let serializer = js_serializer();
    result.serialize(&serializer).map_err(|err| {
        err_to_js(ErrorReport::invalid_input(format!(
            "find_references result is not JS-serialisable: {err}"
        )))
    })
}

fn find_references_internal(
    sources: &HashMap<String, String>,
    entry: &str,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Option<Vec<ReferenceLocation>> {
    let source = sources.get(entry)?;
    let workspace = build_workspace(sources, entry, source);
    let tree = workspace.modules.get(entry)?;
    let root = workspace.nodes.get(entry)?;
    let ranges = relon_analyzer::references::resolve(
        source,
        root,
        tree,
        line,
        character,
        include_declaration,
    )?;
    Some(
        ranges
            .into_iter()
            .map(|r| ReferenceLocation {
                start_line: r.start.line,
                start_character: r.start.column as u32,
                end_line: r.end.line,
                end_character: r.end.column as u32,
                start_offset: r.start.offset as u32,
                end_offset: r.end.offset as u32,
            })
            .collect(),
    )
}

/// Build the workspace tree for the given entry using the same
/// in-memory + std loader chain as `goto_definition` / `complete`.
/// Pulled out so hover / signature_help share the construction.
fn build_workspace(
    sources: &HashMap<String, String>,
    entry: &str,
    source: &str,
) -> relon_analyzer::workspace::WorkspaceTree {
    let in_memory: Arc<dyn ModuleResolver> = Arc::new(InMemoryModuleResolver::new(sources.clone()));
    let std_resolver: Arc<dyn ModuleResolver> = Arc::new(StdModuleResolver);
    let entry_dir = parent_dir(entry);
    let entry_dir_path = PathBuf::from(if entry_dir.is_empty() {
        ".".to_string()
    } else {
        entry_dir.clone()
    });
    let mut loader = ResolverChainLoader::from_resolvers(vec![in_memory, std_resolver]);
    relon_analyzer::workspace::analyze_entry(entry.to_string(), source, entry_dir_path, &mut loader)
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
    /// Snippet template inserted when the user accepts the
    /// suggestion. Uses CodeMirror / LSP-style `${N:placeholder}`
    /// tab-stop syntax. `None` means insert the bare `label`.
    /// Populated for callables (decorators, methods, stdlib fns) so
    /// `Tab` on `@currency` expands to `@currency(${1:symbol})`.
    pub apply_snippet: Option<String>,
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
    let serializer = js_serializer();
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

    // Same loader chain as goto_definition — in-memory first, std
    // fallback — so `#import` resolution sees the playground's tabs.
    let in_memory: Arc<dyn ModuleResolver> = Arc::new(InMemoryModuleResolver::new(sources.clone()));
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

    // Two tiers, post-P6:
    //   1. Workspace analysis succeeded → full scope-aware completion
    //      with cross-module visibility.
    //   2. Workspace failed (entry didn't parse cleanly) → fall back
    //      to a partial-AST parse via the recovering API and route
    //      it through the analyzer's recovering completion path.
    //      The partial AST gives us bare / member context awareness
    //      even for `#`, `&`, `@`, `{`, `f"…${`, ... — anywhere the
    //      user is mid-typing. The keywords-only fallback is gone.
    let items: Vec<complete::CompletionItem> =
        match (workspace.modules.get(entry), workspace.nodes.get(entry)) {
            (Some(tree), Some(root)) => {
                complete::resolve(source, root, tree, Some(&workspace), line, character)
            }
            _ => {
                let parsed = parse_document_recovering(source);
                complete::resolve_recovering(source, &parsed, line, character)
            }
        };

    Some(items.into_iter().map(into_result).collect())
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
        apply_snippet: item.apply_snippet,
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
            r#"#relaxed
#import lib from "./lib.relon"
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
