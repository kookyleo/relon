//! Synchronous LSP server built on `lsp-server`.
//!
//! Lifecycle:
//!
//! 1. `run_stdio()` opens a stdio transport and waits for `initialize`.
//! 2. After `initialized`, the server enters a message loop.
//! 3. On `textDocument/didOpen` and `textDocument/didChange` we (re)parse,
//!    (re)analyze, and publish diagnostics for the affected document.
//! 4. On `shutdown` / `exit` we drain and return cleanly.
//!
//! Document state is held in a `DocumentStore`. Currently we only need
//! the latest source per URI; future passes (hover, go-to-definition)
//! will also cache the latest `AnalyzedTree`.
//!
//! The server is deliberately single-threaded — Relon files are small
//! and analysis is cheap, so the request loop runs everything inline.

use crate::diagnostics::batch_to_lsp;
use crate::features;
use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{Completion, GotoDefinition, HoverRequest, Request as _};
use lsp_types::{
    CompletionOptions, GotoDefinitionParams, GotoDefinitionResponse, HoverParams, InitializeParams,
    OneOf, PublishDiagnosticsParams, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, Url,
};
use relon_analyzer::{analyze, AnalyzedTree};
use relon_parser::{parse_document, Node};
use std::collections::HashMap;
use std::sync::Arc;

/// Run the server over stdio. Returns when the client sends `exit`.
pub fn run_stdio() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    run_with_connection(connection)?;
    io_threads.join().context("join I/O threads")?;
    Ok(())
}

/// Drive a server using an already-constructed `Connection`. Useful for
/// in-process testing.
pub fn run_with_connection(connection: Connection) -> Result<()> {
    let server_capabilities = serde_json::to_value(&ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        // Static features powered by `relon-analyzer` side-tables.
        // Kept simple — no resolveProvider, no triggerCharacters yet.
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions::default()),
        ..ServerCapabilities::default()
    })?;
    let initialize_params = connection.initialize(server_capabilities)?;
    let _params: InitializeParams =
        serde_json::from_value(initialize_params).context("deserialize InitializeParams")?;

    let mut state = ServerState::default();
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                handle_request(&connection, &mut state, req)?;
            }
            Message::Notification(notif) => {
                handle_notification(&connection, &mut state, notif)?;
            }
            Message::Response(_) => {
                // Server-initiated requests not yet implemented; drop
                // any responses we receive.
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct ServerState {
    docs: DocumentStore,
}

/// One document's cached state. We pay parse + analyze on every edit
/// (relon files are small) so requests don't have to re-do the work.
///
/// Public so feature handlers in `crate::features` can name it; the
/// fields are also `pub` since each handler reads multiple of them.
pub struct DocumentEntry {
    pub source: String,
    pub root: Arc<Node>,
    pub tree: Arc<AnalyzedTree>,
}

#[derive(Default)]
struct DocumentStore {
    docs: HashMap<Url, DocumentEntry>,
}

impl DocumentStore {
    fn upsert(&mut self, uri: Url, text: String) {
        let entry = build_entry(text);
        self.docs.insert(uri, entry);
    }

    fn remove(&mut self, uri: &Url) {
        self.docs.remove(uri);
    }

    fn get(&self, uri: &Url) -> Option<&DocumentEntry> {
        self.docs.get(uri)
    }
}

/// Parse + analyze a source, packaging the result for the document
/// store. `root` and `tree` are wrapped in `Arc` so request handlers
/// can hold them without a lifetime tied to `&ServerState`.
fn build_entry(source: String) -> DocumentEntry {
    let root = match parse_document(&source) {
        Ok(node) => Arc::new(node),
        Err(_) => {
            // Synthesize an empty dict so the entry is still usable
            // (diagnostics handle the parse error separately).
            Arc::new(empty_document())
        }
    };
    let tree = Arc::new(analyze(&root));
    DocumentEntry { source, root, tree }
}

fn empty_document() -> Node {
    use relon_parser::{Expr, TokenPosition, TokenRange};
    let zero = TokenPosition::default();
    Node::new(
        Expr::Dict(Vec::new()),
        TokenRange {
            start: zero,
            end: zero,
        },
    )
}

fn handle_request(conn: &Connection, state: &mut ServerState, req: Request) -> Result<()> {
    let id = req.id.clone();
    let method = req.method.clone();
    let response = match req.method.as_str() {
        GotoDefinition::METHOD => {
            let params: GotoDefinitionParams = serde_json::from_value(req.params)?;
            let uri = params
                .text_document_position_params
                .text_document
                .uri
                .clone();
            let position = params.text_document_position_params.position;
            let result = state
                .docs
                .get(&uri)
                .and_then(|entry| features::definition::resolve(entry, position, &uri))
                .map(GotoDefinitionResponse::Scalar);
            ok_response(id, &result)?
        }
        HoverRequest::METHOD => {
            let params: HoverParams = serde_json::from_value(req.params)?;
            let uri = params
                .text_document_position_params
                .text_document
                .uri
                .clone();
            let position = params.text_document_position_params.position;
            let hover = state
                .docs
                .get(&uri)
                .and_then(|entry| features::hover::compute(entry, position));
            ok_response(id, &hover)?
        }
        Completion::METHOD => {
            let params: lsp_types::CompletionParams = serde_json::from_value(req.params)?;
            let uri = params.text_document_position.text_document.uri.clone();
            let position = params.text_document_position.position;
            let items = state
                .docs
                .get(&uri)
                .map(|entry| features::completion::items_for(entry, position))
                .unwrap_or_default();
            let response = if items.is_empty() {
                None
            } else {
                Some(lsp_types::CompletionResponse::Array(items))
            };
            ok_response(id, &response)?
        }
        _ => Response {
            id,
            result: None,
            error: Some(lsp_server::ResponseError {
                code: lsp_server::ErrorCode::MethodNotFound as i32,
                message: format!("method `{method}` not implemented"),
                data: None,
            }),
        },
    };
    conn.sender.send(Message::Response(response))?;
    Ok(())
}

fn ok_response<T: serde::Serialize>(id: lsp_server::RequestId, value: &T) -> Result<Response> {
    Ok(Response {
        id,
        result: Some(serde_json::to_value(value)?),
        error: None,
    })
}

fn handle_notification(
    conn: &Connection,
    state: &mut ServerState,
    notif: Notification,
) -> Result<()> {
    match notif.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let params: lsp_types::DidOpenTextDocumentParams =
                serde_json::from_value(notif.params)?;
            let uri = params.text_document.uri.clone();
            state
                .docs
                .upsert(uri.clone(), params.text_document.text.clone());
            publish_diagnostics(conn, &state.docs, &uri)?;
        }
        DidChangeTextDocument::METHOD => {
            let params: lsp_types::DidChangeTextDocumentParams =
                serde_json::from_value(notif.params)?;
            // `TextDocumentSyncKind::FULL` means each change carries
            // the entire new document text in `content_changes[0]`.
            if let Some(change) = params.content_changes.into_iter().next() {
                let uri = params.text_document.uri.clone();
                state.docs.upsert(uri.clone(), change.text);
                publish_diagnostics(conn, &state.docs, &uri)?;
            }
        }
        DidCloseTextDocument::METHOD => {
            let params: lsp_types::DidCloseTextDocumentParams =
                serde_json::from_value(notif.params)?;
            state.docs.remove(&params.text_document.uri);
            // Clear diagnostics on close.
            send_diagnostics(conn, params.text_document.uri, vec![])?;
        }
        _ => {
            // Unknown notification — silently ignore (per LSP spec).
        }
    }
    Ok(())
}

fn publish_diagnostics(conn: &Connection, docs: &DocumentStore, uri: &Url) -> Result<()> {
    let Some(entry) = docs.get(uri) else {
        return Ok(());
    };
    // Diagnostics combine analyzer findings with parser failures; the
    // latter only show up if `build_entry` swapped in the empty
    // document. Reuse the streaming `compute_diagnostics` helper so
    // both paths stay in lockstep.
    let diags = compute_diagnostics(&entry.source);
    send_diagnostics(conn, uri.clone(), diags)
}

fn send_diagnostics(
    conn: &Connection,
    uri: Url,
    diagnostics: Vec<lsp_types::Diagnostic>,
) -> Result<()> {
    let params = PublishDiagnosticsParams {
        uri,
        diagnostics,
        version: None,
    };
    let notif = Notification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(params)?,
    };
    conn.sender.send(Message::Notification(notif))?;
    Ok(())
}

/// Run parse + analyze and convert the analyzer's diagnostics to LSP.
/// Parser errors are reported as a single LSP diagnostic at the parse
/// failure site.
pub fn compute_diagnostics(source: &str) -> Vec<lsp_types::Diagnostic> {
    let node = match parse_document(source) {
        Ok(n) => n,
        Err(err) => {
            return vec![parse_error_to_diagnostic(err, source)];
        }
    };
    let tree = analyze(&node);
    batch_to_lsp(&tree.diagnostics, source)
}

fn parse_error_to_diagnostic(
    err: relon_parser::ParseDocumentError,
    source: &str,
) -> lsp_types::Diagnostic {
    let span = err
        .source_span()
        .unwrap_or_else(|| miette::SourceSpan::from((0, 1)));
    let range = miette_span_to_lsp_range(span, source);
    lsp_types::Diagnostic {
        range,
        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String(
            "relon::parse".to_string(),
        )),
        code_description: None,
        source: Some("relon".to_string()),
        message: err.to_string(),
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Translate a miette span to an LSP range using the shared
/// position-translation helpers.
fn miette_span_to_lsp_range(span: miette::SourceSpan, source: &str) -> lsp_types::Range {
    use crate::position::offset_to_position;
    lsp_types::Range {
        start: offset_to_position(source, span.offset()),
        end: offset_to_position(source, span.offset() + span.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_diagnostics_returns_empty_for_clean_source() {
        let diags = compute_diagnostics(
            r#"{
                #schema User { String name: * },
                User alice: { name: "A" }
            }"#,
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn compute_diagnostics_reports_schema_errors() {
        let diags = compute_diagnostics(r#"{ #schema Bad 42 }"#);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].severity,
            Some(lsp_types::DiagnosticSeverity::ERROR)
        );
    }

    #[test]
    fn compute_diagnostics_reports_parse_errors() {
        let diags = compute_diagnostics("{ a: }");
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("parse"));
    }
}
