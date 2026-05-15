//! Cursor-position function signature hints.
//!
//! When the cursor sits inside a function call's argument list — e.g.
//! `currency(│)` or `len(items, │)` — surface the callee's parameter
//! list as a tooltip, with the active parameter index (so the IDE can
//! bold the one being typed).

use crate::sig::{lookup_signature_path, FnSignature};
use crate::tree::AnalyzedTree;
use relon_parser::{Expr, Node, TokenRange};
use std::collections::HashMap;

/// One signature plus the index of the parameter the cursor is on.
#[derive(Debug, Clone)]
pub struct SignatureHelp {
    pub signature: String,
    /// Zero-indexed; clamped to `params.len()`. Always populated even
    /// when the call is at the empty `(│)` form (active = 0).
    pub active_parameter: usize,
    /// Source range of the call so the IDE can attach the tooltip.
    pub range: TokenRange,
}

/// Resolve a signature-help request at `(line, character)`. Returns
/// `None` when the cursor isn't inside a callable invocation.
pub fn resolve(
    source: &str,
    root: &Node,
    tree: &AnalyzedTree,
    line: u32,
    character: u32,
) -> Option<SignatureHelp> {
    let offset = crate::goto_def::position_to_offset(source, line, character);
    let call = enclosing_call(root, offset)?;
    let (path, args, call_range) = match &*call.expr {
        Expr::FnCall { path, args } => (path.clone(), args.clone(), call.range),
        _ => return None,
    };

    let name_segments: Vec<String> = path
        .iter()
        .filter_map(|seg| match seg {
            relon_parser::TokenKey::String(s, _, _) => Some(s.clone()),
            _ => None,
        })
        .collect();
    if name_segments.is_empty() {
        return None;
    }
    let dotted = name_segments.join(".");

    let host: HashMap<String, FnSignature> = HashMap::new();
    let sig = lookup_signature_path(&name_segments, tree, &host)?;
    let rendered = render_signature(&sig);
    let active = active_param_index(source, offset, &args, call_range);
    let max_idx = sig.params.len().saturating_sub(1);

    Some(SignatureHelp {
        signature: format!("{dotted}{rendered}"),
        active_parameter: active.min(max_idx),
        range: call_range,
    })
}

fn enclosing_call(root: &Node, offset: usize) -> Option<&Node> {
    fn visit<'a>(node: &'a Node, offset: usize, best: &mut Option<&'a Node>) {
        let r = node.range;
        if offset < r.start.offset || offset > r.end.offset {
            return;
        }
        if matches!(&*node.expr, Expr::FnCall { .. }) {
            *best = Some(node);
        }
        for child in relon_parser::child_nodes(node) {
            visit(child, offset, best);
        }
    }
    let mut best = None;
    visit(root, offset, &mut best);
    best
}

fn render_signature(sig: &FnSignature) -> String {
    let params: Vec<String> = sig
        .params
        .iter()
        .map(|p| {
            let ty = format_type(&p.ty);
            let opt = if p.optional { "?" } else { "" };
            format!("{}{}: {}", p.name, opt, ty)
        })
        .collect();
    let tail = match &sig.variadic_tail {
        Some(t) => format!(", ...{}", format_type(t)),
        None => String::new(),
    };
    format!(
        "({}{}) -> {}",
        params.join(", "),
        tail,
        format_type(&sig.return_type)
    )
}

fn format_type(t: &relon_parser::TypeNode) -> String {
    let suffix = if t.is_optional { "?" } else { "" };
    let path = t.path.join(".");
    if t.generics.is_empty() {
        format!("{path}{suffix}")
    } else {
        let inner: Vec<String> = t.generics.iter().map(format_type).collect();
        format!("{path}<{}>{suffix}", inner.join(", "))
    }
}

/// Count completed args before `offset`. Walks the parsed CallArg
/// list and increments for each whose end-offset is before the
/// cursor — close to what LSP clients use to highlight the
/// "current" parameter.
fn active_param_index(
    source: &str,
    offset: usize,
    args: &[relon_parser::CallArg],
    call_range: TokenRange,
) -> usize {
    // The CallArg shape doesn't carry a per-arg range we can rely on
    // for every variant (some are bare values, some are named). Fall
    // back to scanning commas inside the call's source slice up to
    // `offset` — a cheap, robust signal that works on partial parses.
    let start = call_range.start.offset.min(source.len());
    let end = offset.min(source.len()).max(start);
    let slice = &source[start..end];
    let mut depth: i32 = 0;
    let mut commas = 0usize;
    let mut saw_open_paren = false;
    for ch in slice.chars() {
        match ch {
            '(' | '[' | '{' => {
                if ch == '(' && !saw_open_paren {
                    saw_open_paren = true;
                    continue;
                }
                depth += 1;
            }
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 && saw_open_paren => commas += 1,
            _ => {}
        }
    }
    let _ = args; // future: use names to disambiguate named args
    commas
}
