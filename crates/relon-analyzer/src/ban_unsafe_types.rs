//! Type-surface bans: `Any` (v1.6), bare generics (v1.7),
//! and removed/internal type names (`Null` / `Unit` / `Enum`).
//!
//! v1.5 still treated `Any` as a regular builtin type — users could
//! write `Any payload: ...` or `#main(Any x)` and the analyzer would
//! happily pass it through, defeating every other strict-mode
//! guarantee downstream. v1.6 retires `Any` from the user-facing
//! surface entirely; v1.7 closes the related back-door of bare
//! generic containers (`List` / `Dict` / `Closure` / `Fn` without explicit
//! type arguments, which used to silently expand to `<Any>` shapes); v1.8
//! extends both bans to host-supplied
//! signatures via `audit_host_fn_signatures` in `lib.rs`.
//!
//! Every site where a user-written `TypeNode` appears routes through
//! [`scan_typenode_for_any`], which walks the node tree (including
//! nested generics) and pushes [`Diagnostic::ExplicitAnyForbidden`]
//! for each single-segment `Any` token,
//! [`Diagnostic::ReservedTypeName`] for each user-written `Null` / `Unit` / `Enum`,
//! and [`Diagnostic::BareGenericContainer`] for each bare-generic container.
//!
//! Internal-only `Any` (the analyzer's `InferredType::Any`
//! placeholder for "unknown" / "couldn't infer") is *not* affected.
//! The ban is purely on user source / host signatures — once those
//! walks are clean, all downstream `Any` is the analyzer's own
//! escape hatch, and v1.4 / v1.5 strict checks already catch the
//! cases where it leaks.

use crate::diagnostic::{span_of, Diagnostic};
use relon_parser::TypeNode;

/// Walk `t` (and every nested generic argument) and push
/// [`Diagnostic::ExplicitAnyForbidden`] for each occurrence of a
/// single-segment `Any` head. `context` is rendered into the
/// diagnostic message so the user sees which source position the ban
/// fired on (e.g. `"schema field \`port\`"`,
/// `"#main parameter \`x\`"`, `"closure parameter \`n\`"`).
///
/// The walk descends into `generics` recursively so
/// `List<Dict<String, Any>>` (Any nested two levels deep) still gets
/// flagged. Multi-segment paths (`pkg.Any`) are intentionally ignored —
/// users may legitimately have a schema named `Any` in another module.
pub(crate) fn scan_typenode_for_any(t: &TypeNode, context: &str, out: &mut Vec<Diagnostic>) {
    if t.path.len() == 1 && t.path[0] == "Any" {
        out.push(Diagnostic::ExplicitAnyForbidden {
            context: context.to_string(),
            range: span_of(t.range),
        });
    }
    if t.path.len() == 1 && matches!(t.path[0].as_str(), "Null" | "Unit" | "Enum") {
        out.push(Diagnostic::ReservedTypeName {
            type_name: t.path[0].clone(),
            context: context.to_string(),
            range: span_of(t.range),
        });
    }
    // v1.7: bare-generic check piggybacks on the same walk. Bare
    // `List` / `Dict` / `Closure` / `Fn` (no generic args)
    // is the back-door equivalent of writing `<Any>` everywhere — the
    // pre-v1.7 `infer_from_type_node` quietly turned them into
    // `List<Any>` / `Dict<Any, Any>` / `Fn(...) -> Any` / `Any`. By
    // routing every user-side TypeNode through this walker, v1.7
    // forces an explicit type parameter at each occurrence.
    if t.path.len() == 1 && t.generics.is_empty() {
        let name = t.path[0].as_str();
        if matches!(name, "List" | "Dict" | "Closure" | "Fn") {
            out.push(Diagnostic::BareGenericContainer {
                type_name: name.to_string(),
                context: context.to_string(),
                range: span_of(t.range),
            });
        }
    }
    for inner in &t.generics {
        scan_typenode_for_any(inner, context, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sig::{type_node_generic, type_node_simple};

    /// Single-segment `Any` is reported once.
    #[test]
    fn flat_any_reported() {
        let mut out = Vec::new();
        scan_typenode_for_any(&type_node_simple("Any"), "test", &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Diagnostic::ExplicitAnyForbidden { .. }));
    }

    /// Single-segment non-`Any` type is silent.
    #[test]
    fn flat_int_silent() {
        let mut out = Vec::new();
        scan_typenode_for_any(&type_node_simple("Int"), "test", &mut out);
        assert!(out.is_empty());
    }

    /// Removed/internal names are rejected as user-written type names.
    #[test]
    fn null_unit_and_enum_report_reserved_type_name() {
        let mut out = Vec::new();
        scan_typenode_for_any(&type_node_simple("Null"), "test", &mut out);
        scan_typenode_for_any(&type_node_simple("Unit"), "test", &mut out);
        scan_typenode_for_any(
            &type_node_generic("Enum", vec![type_node_simple("Int")]),
            "test",
            &mut out,
        );
        assert_eq!(out.len(), 3);
        assert!(
            matches!(out[0], Diagnostic::ReservedTypeName { ref type_name, .. } if type_name == "Null")
        );
        assert!(
            matches!(out[1], Diagnostic::ReservedTypeName { ref type_name, .. } if type_name == "Unit")
        );
        assert!(
            matches!(out[2], Diagnostic::ReservedTypeName { ref type_name, .. } if type_name == "Enum")
        );
    }

    /// `List<Any>` reports once on the inner.
    #[test]
    fn list_of_any_reported() {
        let mut out = Vec::new();
        scan_typenode_for_any(
            &type_node_generic("List", vec![type_node_simple("Any")]),
            "test",
            &mut out,
        );
        assert_eq!(out.len(), 1);
    }

    /// `List<Int>` is silent.
    #[test]
    fn list_of_int_silent() {
        let mut out = Vec::new();
        scan_typenode_for_any(
            &type_node_generic("List", vec![type_node_simple("Int")]),
            "test",
            &mut out,
        );
        assert!(out.is_empty());
    }

    /// `Dict<String, Any>` reports on the value slot.
    #[test]
    fn dict_string_any_reported() {
        let mut out = Vec::new();
        scan_typenode_for_any(
            &type_node_generic(
                "Dict",
                vec![type_node_simple("String"), type_node_simple("Any")],
            ),
            "test",
            &mut out,
        );
        assert_eq!(out.len(), 1);
    }

    /// `List<Dict<String, Any>>` (Any nested two levels deep) is
    /// still caught.
    #[test]
    fn deeply_nested_any_reported() {
        let mut out = Vec::new();
        let nested = type_node_generic(
            "List",
            vec![type_node_generic(
                "Dict",
                vec![type_node_simple("String"), type_node_simple("Any")],
            )],
        );
        scan_typenode_for_any(&nested, "test", &mut out);
        assert_eq!(out.len(), 1);
    }

    /// Multi-segment `pkg.Any` is *not* flagged (users may legitimately
    /// have a schema named `Any` in a module).
    #[test]
    fn multi_segment_any_silent() {
        let mut out = Vec::new();
        let mut t = type_node_simple("pkg");
        t.path.push("Any".to_string());
        scan_typenode_for_any(&t, "test", &mut out);
        assert!(out.is_empty());
    }

    /// The diagnostic carries the supplied context string.
    #[test]
    fn context_string_propagates() {
        let mut out = Vec::new();
        scan_typenode_for_any(&type_node_simple("Any"), "schema field `port`", &mut out);
        if let Diagnostic::ExplicitAnyForbidden { context, .. } = &out[0] {
            assert_eq!(context, "schema field `port`");
        } else {
            panic!("wrong variant");
        }
    }
}
