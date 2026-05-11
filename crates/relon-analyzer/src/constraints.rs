//! Schema-rooted Phase C: constraint registry + `#derive Constraint`
//! witness shape checking + Equatable / JsonProjectable auto-derive.
//!
//! Decisions 17 / 18 / 19 of `schema-rooted-model-2026-05-11.md`:
//!
//! * **Nominal trait** — constraints are identified by name; whether
//!   a schema satisfies one is decided either by explicit `#derive`
//!   on a method whose name + signature matches the witness shape,
//!   or by analyzer-side auto-derive of structural constraints
//!   (decision 19, default-ON list).
//! * **Operator lowering** — `==` / `!=` go through `Equatable::eq`;
//!   `<` / `>` go through `Comparable::lt`; `<=` / `>=` are
//!   synthesized from `lt` + `eq`.
//! * **Default derive list** — `{Equatable, JsonProjectable}`. Users
//!   opt out with `#no_auto_derive X`. `Comparable` is *not*
//!   auto-derived (asymmetry rationale: comparison is rarely
//!   meaningful by default).
//!
//! This module owns:
//! 1. The static registry mapping `Constraint -> ExpectedWitness`.
//! 2. A pass that walks every method's `derives` list and reports
//!    `ConstraintWitnessShapeMismatch` when the witness method's
//!    name / params / return type don't match the constraint's
//!    expected shape.
//! 3. A pass that synthesizes auto-derived methods (`eq`,
//!    `to_json`) onto each user schema that hasn't opted out and
//!    doesn't already provide a user method of that name.
//!
//! Constraints that need lowering for `()`/index/call/numeric
//! operators (Iterable / Indexable / Callable / Number) are not yet
//! registered here — they require evaluator-side work that is out of
//! scope for the C.3-C.5 batch and will be filled in later.

use crate::diagnostic::{span_of, Diagnostic};
use crate::schema::{SchemaMethodInfo, SchemaMethodParamInfo};
use crate::sig::{type_node_simple, FnParam, FnSignature};
use crate::tree::AnalyzedTree;
use crate::typecheck::format_type;
use relon_parser::TypeNode;

/// Expected witness method shape for a constraint. Owners of multiple
/// witnesses would extend this to `Vec<ExpectedWitness>`; first-version
/// constraints (`Equatable`, `Comparable`, `JsonProjectable`) have
/// exactly one witness each.
#[derive(Debug, Clone)]
pub(crate) struct ExpectedWitness {
    /// Constraint name as users write it in `#derive Constraint`.
    pub constraint: &'static str,
    /// Method name the witness must have.
    pub method: &'static str,
    /// Parameter names + expected types. `Self` placeholder is matched
    /// loosely — any path whose single segment is `Self` is accepted
    /// (the parser preserves the literal token).
    pub params: &'static [ExpectedParam],
    /// Expected return type (single-segment builtin name).
    pub return_type: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct ExpectedParam {
    pub name: &'static str,
    /// Single-segment type the param must declare. `"Self"` matches
    /// only literal `Self`; concrete builtins (`"Bool"`, `"String"`,
    /// ...) match by name equality.
    pub type_name: &'static str,
}

/// Built-in constraint registry. Adding a new constraint here is the
/// only knob analyzer / evaluator code needs to flip when the
/// language picks up new witness-driven operators.
pub(crate) const CONSTRAINTS: &[ExpectedWitness] = &[
    ExpectedWitness {
        constraint: "Equatable",
        method: "eq",
        params: &[ExpectedParam {
            name: "other",
            type_name: "Self",
        }],
        return_type: "Bool",
    },
    ExpectedWitness {
        constraint: "Comparable",
        method: "lt",
        params: &[ExpectedParam {
            name: "other",
            type_name: "Self",
        }],
        return_type: "Bool",
    },
    ExpectedWitness {
        constraint: "JsonProjectable",
        method: "to_json",
        params: &[],
        return_type: "String",
    },
    // Future constraints (Iterable, Indexable, Callable, Number) need
    // evaluator-side lowering for ()/index/call/numeric operators and
    // are deferred — see schema-rooted-implementation-log §C.3.
];

/// Look up a constraint by name. Returns `None` for names that aren't
/// registered yet (Iterable / Indexable / Callable / Number per the
/// module-level note).
fn lookup_constraint(name: &str) -> Option<&'static ExpectedWitness> {
    CONSTRAINTS.iter().find(|c| c.constraint == name)
}

/// Render an `ExpectedWitness` for the diagnostic's `expected_shape`
/// slot. Output mirrors source-level Relon: `eq(other: Self) -> Bool`.
fn format_expected_shape(w: &ExpectedWitness) -> String {
    let params: Vec<String> = w
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, p.type_name))
        .collect();
    format!("{}({}) -> {}", w.method, params.join(", "), w.return_type)
}

/// Render the actual method as the user wrote it, for the diagnostic's
/// `found_shape` slot. Mirrors `format_expected_shape` so the operator
/// can eyeball the diff.
fn format_actual_shape(m: &SchemaMethodInfo) -> String {
    let params: Vec<String> = m
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, format_type(&p.type_node)))
        .collect();
    format!(
        "{}({}) -> {}",
        m.name,
        params.join(", "),
        format_type(&m.return_type)
    )
}

/// True when `actual` matches a `Self` placeholder *or* the concrete
/// `schema` name — both forms appear in source (`Self` in the witness
/// declaration, the spelled-out schema when the user is explicit).
fn type_matches_self_or_schema(actual: &TypeNode, schema: &str) -> bool {
    !actual.is_optional
        && actual.generics.is_empty()
        && actual.path.len() == 1
        && (actual.path[0] == "Self" || actual.path[0] == schema)
}

/// True when `actual` is a bare single-segment type with the given name
/// (no generics, no optional).
fn type_matches_named(actual: &TypeNode, name: &str) -> bool {
    !actual.is_optional
        && actual.generics.is_empty()
        && actual.path.len() == 1
        && actual.path[0] == name
}

/// Decide whether `actual_param.type_node` matches `expected.type_name`.
fn param_type_matches(
    expected: &ExpectedParam,
    actual: &SchemaMethodParamInfo,
    schema: &str,
) -> bool {
    if expected.type_name == "Self" {
        type_matches_self_or_schema(&actual.type_node, schema)
    } else {
        type_matches_named(&actual.type_node, expected.type_name)
    }
}

/// Decide whether `actual` return type matches `expected_return`.
fn return_type_matches(actual: &TypeNode, expected_return: &str) -> bool {
    type_matches_named(actual, expected_return)
}

/// Walk every method on every schema, find `#derive C` pragmas, and
/// emit `ConstraintWitnessShapeMismatch` when the method's shape
/// doesn't match the registered witness for `C`.
///
/// Methods that don't carry a `#derive` pragma are not checked here —
/// they may coincidentally share names with witnesses (e.g. a user's
/// `eq()` that isn't intended to be an `Equatable` witness) and the
/// analyzer's nominal-trait stance is "only what's explicitly
/// derived counts".
pub fn check_derive_witnesses(tree: &mut AnalyzedTree) {
    let mut diags = Vec::new();
    for (schema_name, methods) in &tree.schema_methods {
        for method in methods {
            for constraint_name in &method.derives {
                let Some(expected) = lookup_constraint(constraint_name) else {
                    // Unknown constraint name: silent for now (Phase C
                    // hasn't introduced `InvalidDeriveTarget` yet; see
                    // type-constraints-spec §"错误类型草案"). The parser
                    // would have rejected garbage at the lex level.
                    continue;
                };
                let ok = method.name == expected.method
                    && method.params.len() == expected.params.len()
                    && method
                        .params
                        .iter()
                        .zip(expected.params.iter())
                        .all(|(actual, exp)| param_type_matches(exp, actual, schema_name))
                    && return_type_matches(&method.return_type, expected.return_type);
                if !ok {
                    diags.push(Diagnostic::ConstraintWitnessShapeMismatch {
                        constraint: constraint_name.clone(),
                        method: method.name.clone(),
                        expected_shape: format_expected_shape(expected),
                        found_shape: format_actual_shape(method),
                        range: span_of(method.name_range),
                    });
                }
            }
        }
    }
    tree.diagnostics.extend(diags);
}

/// Auto-derive structural constraints onto user schemas. Decision 19:
/// `Equatable` and `JsonProjectable` are default-ON; users opt out per
/// constraint via `#no_auto_derive C` inside the schema's `with` block.
/// `Comparable` is *not* auto-derived.
///
/// Synthesized methods carry `is_native = true` + `body_node = None`.
/// The evaluator recognizes auto-derived names that have no
/// `native_methods` entry and falls back to its built-in
/// `Value::PartialEq` / `serde_json` implementations (see
/// `arithmetic::try_compare_op_method` and `Value::to_json_string`).
///
/// Must run before [`crate::extend::build_method_signature_table`] so
/// the synthesized methods land in `method_signatures` too.
pub fn auto_derive_schemas(tree: &mut AnalyzedTree) {
    // Snapshot the set of (schema_name, opt_outs) so we don't mutate
    // while iterating. Root-level `#schema Name Body` directives are
    // lowered into `tree.schemas` too (keyed by the body node's id),
    // so `tree.schemas` is the single authoritative source for both
    // forms and their `schema_no_auto_derives` payload.
    let mut schema_opt_outs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for def in tree.schemas.values() {
        let Some(name) = def.name.as_ref() else {
            continue;
        };
        schema_opt_outs
            .entry(name.clone())
            .or_default()
            .extend(def.schema_no_auto_derives.iter().cloned());
    }

    // Iterate the *declared* schema names. A user schema appears here
    // when at least one `#schema X { ... }` lowered it into
    // `tree.schemas` — `#extend`-only contributions to built-ins
    // (`String`, `Int`, ...) are not user-defined schemas and should
    // not get auto-derived methods.
    let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
    for def in tree.schemas.values() {
        if let Some(name) = def.name.as_ref() {
            declared.insert(name.clone());
        }
    }

    for schema_name in declared {
        let opt_outs = schema_opt_outs
            .get(&schema_name)
            .cloned()
            .unwrap_or_default();
        // Existing methods (whether from `#schema` or `#extend`) take
        // precedence — never shadow what the user wrote.
        let existing_methods: std::collections::HashSet<String> = tree
            .schema_methods
            .get(&schema_name)
            .map(|methods| methods.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default();

        for derive_target in AUTO_DERIVE_LIST {
            if opt_outs.iter().any(|o| o == derive_target.constraint) {
                continue;
            }
            if existing_methods.contains(derive_target.method) {
                continue;
            }
            // Synthesize the method entry.
            let synthesized = synthesize_auto_method(derive_target);
            tree.schema_methods
                .entry(schema_name.clone())
                .or_default()
                .push(synthesized);
        }
    }
}

/// Constraint names this pass auto-injects. Comparable is deliberately
/// absent — see decision 19 + module-level docs.
const AUTO_DERIVE_LIST: &[&ExpectedWitness] = &[
    &CONSTRAINTS[0], // Equatable -> eq
    &CONSTRAINTS[2], // JsonProjectable -> to_json
];

/// Build an analyzer-side `SchemaMethodInfo` for an auto-derived
/// constraint witness. `is_native = true` flags the evaluator to use
/// its built-in fallback (PartialEq / serde_json) rather than look up
/// a registered native method that doesn't exist.
fn synthesize_auto_method(target: &ExpectedWitness) -> SchemaMethodInfo {
    let params = target
        .params
        .iter()
        .map(|p| SchemaMethodParamInfo {
            name: p.name.to_string(),
            name_range: relon_parser::TokenRange::default(),
            type_node: type_node_simple(p.type_name),
        })
        .collect();
    SchemaMethodInfo {
        name: target.method.to_string(),
        name_range: relon_parser::TokenRange::default(),
        params,
        return_type: type_node_simple(target.return_type),
        body_node: None,
        derives: vec![target.constraint.to_string()],
        is_native: true,
        is_private: false,
        range: relon_parser::TokenRange::default(),
        source_module: None,
        doc_comment: None,
    }
}

/// Public synthesizer used by `build_method_signature_table` when it
/// has to recreate a synthetic signature for an auto-derived entry
/// outside `extend::synthesize_method_signature`'s scope. Kept here so
/// the type-name layout stays colocated with the registry.
#[allow(dead_code)]
pub(crate) fn synthesize_signature(schema: &str, target: &ExpectedWitness) -> FnSignature {
    let params = target
        .params
        .iter()
        .map(|p| FnParam {
            name: p.name.to_string(),
            ty: type_node_simple(p.type_name),
            optional: false,
        })
        .collect();
    FnSignature {
        name: format!("{schema}.{}", target.method),
        generics: Vec::new(),
        params,
        return_type: type_node_simple(target.return_type),
        variadic_tail: None,
    }
}
