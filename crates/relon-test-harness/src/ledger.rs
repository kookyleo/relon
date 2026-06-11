//! Coverage ledger for the lowering capability-cap sites.
//!
//! Every loud "unsupported" cap in the lowering pass (each [`cap!`] site,
//! see `relon_ir::lowering::cap::LOWERING_CAP_IDS`) has exactly one
//! [`LedgerEntry`] here. The ledger is the human-auditable companion to
//! the machine registry: it records, per cap id, which family the
//! construct belongs to, whether it is already covered by the real
//! four-way differential or still capped, and an honest one-line reason.
//!
//! The `ledger_completeness` integration test enforces a bijection
//! between [`LEDGER`] and `LOWERING_CAP_IDS` — no orphan ledger row, no
//! unregistered cap — so a future cap added without a ledger entry fails
//! the build.
//!
//! ## Two layers: cap-site registry vs supported-surface coverage
//!
//! There are two distinct things to track, and conflating them would be
//! dishonest, so they live in two tables:
//!
//! * [`LEDGER`] — one row per **cap site**. A cap site is a place in the
//!   lowering pass that *rejects* a shape (`cap!(id, LoweringError…)`).
//!   By construction a cap site only ever fires on a shape it cannot
//!   lower, so **every cap-site row is genuinely [`Status::Capped`]**:
//!   "Covered" is not a meaningful state for a rejection point. The
//!   `status` / `corpus` / `reason` fields record *why* a site rejects
//!   and (for the few caps a test deliberately drives) which probe
//!   exercises it. This is the machine-auditable "no silent new cap"
//!   registry — Wave R6 finalises its reasons but does **not** flip any
//!   site to Covered, because none of them are.
//!
//! * [`SUPPORTED_SURFACE`] — one row per **declared-supported construct**
//!   (Waves R1–R9: contextual-typed closures / comprehension, pipe,
//!   f-string, range / map / filter / reduce, `type()` const-fold,
//!   strict-`match` arm-fold, scalar math, scalar string ops, `is_uuid`,
//!   …). These rows ARE [`Status::Covered`]: each names a real corpus
//!   case that lowers cleanly (the lowering pass fires **no** cap on it)
//!   and runs the compiled backend in the differential. The Wave R6
//!   `no_fallback_over_supported_surface` proof iterates this table and
//!   asserts `auto` reaches the compiled backend for every row — never
//!   the silent tree-walk capability fallback.
//!
//! The honest summary, then: the cap-site ledger is the partition of
//! *rejection points* (all Capped, with truthful reasons); the
//! supported-surface table is the partition of *constructs that lower*
//! (all Covered, each backed by a passing differential case). A
//! construct being Covered does not retire any cap site — the same cap
//! site still guards the *unsupported* corners of that construct family
//! (e.g. f-strings lower, but an f-string interpolating an unlowerable
//! sub-expression still hits an `unsupported_expr` cap).

/// One row per lowering cap id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerEntry {
    /// Stable cap id, matching an entry in
    /// `relon_ir::lowering::cap::LOWERING_CAP_IDS`.
    pub id: &'static str,
    /// Human-readable source location (`file::fn`) for quick navigation.
    /// Deliberately line-number-free so edits above the site don't churn
    /// the ledger.
    pub site: &'static str,
    /// Construct family the cap belongs to.
    pub category: Category,
    /// Whether the construct is already covered by the real four-way
    /// differential today, or still capped.
    pub status: Status,
    /// CorpusCase / probe name that exercises this id, or `""` if none yet.
    pub corpus: &'static str,
    /// Honest one-line reason the construct is capped (or how it is
    /// covered, once flipped).
    pub reason: &'static str,
}

/// Coverage state of a cap id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The construct passes the real four-way differential today.
    Covered,
    /// The construct is rejected with a loud cap; not yet lowerable.
    Capped,
}

/// Construct family a cap belongs to. Mirrors the rough `LoweringError`
/// variant grouping used by the coverage worklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Expression-shape caps (`UnsupportedExpr`, unresolved variables,
    /// stdlib resolution, ternary typing).
    ExprShape,
    /// `#main` signature parameter / return type caps.
    MainSignatureType,
    /// Schema / dict field type caps.
    FieldType,
    /// Operator caps.
    Operator,
    /// Closure-boundary / closure-capture caps.
    Closure,
    /// Cross-module schema merge / brand / default-dependency caps.
    SchemaMerge,
    /// Host-infrastructure caps (missing / duplicate / absent entry).
    HostInfra,
}

/// One entry per lowering cap id. See module docs for the bijection
/// invariant the completeness test enforces.
pub const LEDGER: &[LedgerEntry] = &[
    LedgerEntry {
        id: "lower_workspace.entry_module_not_found.1",
        site: "lowering/mod.rs::lower_workspace",
        category: Category::HostInfra,
        status: Status::Capped,
        corpus: "",
        reason: "host gave an entry id absent from the workspace; not a lowerable construct",
    },
    LedgerEntry {
        id: "lower_workspace.entry_module_not_found.2",
        site: "lowering/mod.rs::lower_workspace",
        category: Category::HostInfra,
        status: Status::Capped,
        corpus: "",
        reason: "host gave an entry id absent from the workspace; not a lowerable construct",
    },
    LedgerEntry {
        id: "lower_workspace.multiple_main_directives",
        site: "lowering/mod.rs::lower_workspace",
        category: Category::HostInfra,
        status: Status::Capped,
        corpus: "",
        reason: "more than one reachable module declares #main; ambiguous entry",
    },
    LedgerEntry {
        id: "detect_cross_file_schema_conflicts.duplicate_schema_across_files",
        site: "lowering/mod.rs::detect_cross_file_schema_conflicts",
        category: Category::SchemaMerge,
        status: Status::Capped,
        corpus: "",
        reason: "same schema name with divergent shapes across modules; no canonical merge",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.missing_main",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::HostInfra,
        status: Status::Capped,
        corpus: "",
        reason: "module has no #main; library/config files are not entry programs",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.closure_across_boundary.1",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::Closure,
        status: Status::Capped,
        corpus: "",
        reason: "closure value would dangle if it crossed the wasm module boundary",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.closure_across_boundary.2",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::Closure,
        status: Status::Capped,
        corpus: "",
        reason: "closure value would dangle if it crossed the wasm module boundary",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.unsupported_expr.1",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.unsupported_expr.2",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.unsupported_type_in_main",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "lower_entry_with_resolver.unsupported_expr.3",
        site: "lowering/mod.rs::lower_entry_with_resolver",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "build_main_params_schema.unsupported_type_in_main",
        site: "lowering/mod.rs::build_main_params_schema",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "build_main_return_schema.unsupported_type_in_main.1",
        site: "lowering/mod.rs::build_main_return_schema",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "build_main_return_schema.unsupported_type_in_main.2",
        site: "lowering/mod.rs::build_main_return_schema",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "return_tuple_canonical.unsupported_type_in_main",
        site: "lowering/mod.rs::return_tuple_canonical",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "Tuple<...> return element outside the T2 first cut (nested tuple / List<...> \
                 element / Unit) — capped pending the cross-region tuple-element work",
    },
    LedgerEntry {
        id: "lower_tuple_return.unsupported_expr",
        site: "lowering/mod.rs::lower_tuple_return / lower_tuple_into_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "tuple-return body is not a tuple literal, or a tuple element lowered to a \
                 type the positional slot cannot store",
    },
    LedgerEntry {
        id: "lower_tuple_return.arity_mismatch",
        site: "lowering/mod.rs::lower_tuple_return",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "tuple literal arity differs from the declared Tuple<...> return arity",
    },
    LedgerEntry {
        id: "desugar_field_decorators.unsupported_expr.1",
        site: "lowering/mod.rs::desugar_field_decorators",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "field decorator path is multi-segment / dynamic — no plain callable to desugar to",
    },
    LedgerEntry {
        id: "desugar_field_decorators.unsupported_expr.2",
        site: "lowering/mod.rs::desugar_field_decorators",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "r11_capped_builtin_value_decorator",
        reason: "builtin @-decorator (@value/@expect/…) has no compiled call form",
    },
    LedgerEntry {
        id: "desugar_field_decorators.unsupported_expr.3",
        site: "lowering/mod.rs::desugar_field_decorators",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "field decorator with a named argument; positional-only call lowering can't thread it",
    },
    LedgerEntry {
        id: "anon_dict_return_plan.unsupported_expr",
        site: "lowering/mod.rs::anon_dict_return_plan",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "anon_dict_return_plan.closure_across_boundary",
        site: "lowering/mod.rs::anon_dict_return_plan",
        category: Category::Closure,
        status: Status::Capped,
        corpus: "",
        reason: "closure value would dangle if it crossed the wasm module boundary",
    },
    LedgerEntry {
        id: "anon_dict_return_plan.unsupported_field_type",
        site: "lowering/mod.rs::anon_dict_return_plan",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "anon_dict_emit_order.cyclic_field_dependency",
        site: "lowering/mod.rs::anon_dict_emit_order",
        category: Category::SchemaMerge,
        status: Status::Capped,
        corpus: "",
        reason: "anon-Dict-return fields form a `&sibling`/`&root` reference cycle (self or \
                 mutually-recursive); the compiled path's loud analogue of the tree-walk \
                 oracle's CircularReference",
    },
    LedgerEntry {
        id: "anon_dict_emit_order.forward_ref_through_param",
        site: "lowering/mod.rs::anon_dict_emit_order",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "forward `&sibling`/`&root` reference into a reference-bearing field whose \
                 component reads a `#main` parameter — the tree-walk oracle loses the param \
                 scope when forcing the forwarded thunk, so this shape is capped to avoid a \
                 silent divergence",
    },
    LedgerEntry {
        id: "classify_anon_dict_scalar_field.unsupported_expr",
        site: "lowering/mod.rs::classify_anon_dict_scalar_field",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_str_int_field.unsupported_expr.1",
        site: "lowering/mod.rs::classify_anon_dict_str_int_field",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_str_int_field.unsupported_expr.2",
        site: "lowering/mod.rs::classify_anon_dict_str_int_field",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_list_string_field.unsupported_expr",
        site: "lowering/mod.rs::classify_anon_dict_list_string_field",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_list_field.unsupported_field_type.1",
        site: "lowering/mod.rs::classify_anon_dict_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "classify_anon_dict_list_field.unsupported_field_type.2",
        site: "lowering/mod.rs::classify_anon_dict_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "classify_anon_dict_list_field.unsupported_field_type.3",
        site: "lowering/mod.rs::classify_anon_dict_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "classify_anon_dict_scalar_field_irt.unsupported_expr.1",
        site: "lowering/mod.rs::classify_anon_dict_scalar_field_irt",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_scalar_field_irt.unsupported_expr.2",
        site: "lowering/mod.rs::classify_anon_dict_scalar_field_irt",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_scalar_field_irt.unsupported_expr.3",
        site: "lowering/mod.rs::classify_anon_dict_scalar_field_irt",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_scalar_field_irt.unsupported_expr.4",
        site: "lowering/mod.rs::classify_anon_dict_scalar_field_irt",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "classify_anon_dict_scalar_field_irt.reference_unresolved",
        site: "lowering/mod.rs::classify_anon_dict_scalar_field_irt",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "R10/R13: forward + backward `&sibling`/`&root` scalar + List<scalar> field \
                 refs lower (corpus `r10_*` / `r13_*`); a `#internal` / non-host-visible sibling \
                 name caps here",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.1",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.2",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.3",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_field_type.1",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.4",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_field_type.2",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.5",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.6",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.7",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_field_type.3",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_anon_dict_body.unsupported_expr.8",
        site: "lowering/mod.rs::lower_anon_dict_body",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "type_repr_to_ir_type.unsupported_type_in_main.1",
        site: "lowering/mod.rs::type_repr_to_ir_type",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "type_repr_to_ir_type.unsupported_type_in_main.2",
        site: "lowering/mod.rs::type_repr_to_ir_type",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "type_repr_to_ir_type_dict.unsupported_type_in_main",
        site: "lowering/mod.rs::type_repr_to_ir_type_dict",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "dict/record field List<…> element outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.1",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.2",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.3",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.4",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.5",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.6",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.7",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.spread_empty",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "list spread flattened to empty; element type cannot be inferred for the \
                 materialiser",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.spread_elem_ty",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "list spread element type is not Int/Float; only scalar Int/Float spread lists \
                 are materialised in the AOT envelope",
    },
    LedgerEntry {
        id: "lower_expr.closure_across_boundary",
        site: "lowering/mod.rs::lower_expr",
        category: Category::Closure,
        status: Status::Capped,
        corpus: "",
        reason: "closure value would dangle if it crossed the wasm module boundary",
    },
    LedgerEntry {
        id: "lower_expr.unsupported_expr.8",
        site: "lowering/mod.rs::lower_expr",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_local_closure_call.unsupported_expr.1",
        site: "lowering/mod.rs::try_lower_local_closure_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_local_closure_call.unknown_stdlib_method",
        site: "lowering/mod.rs::try_lower_local_closure_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "try_lower_local_closure_call.unsupported_expr.2",
        site: "lowering/mod.rs::try_lower_local_closure_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_local_closure_call.unsupported_expr.3",
        site: "lowering/mod.rs::try_lower_local_closure_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_local_closure_call.stdlib_arg_type_mismatch",
        site: "lowering/mod.rs::try_lower_local_closure_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "stdlib call argument type disagrees with the declared signature",
    },
    LedgerEntry {
        id: "try_lower_native_call.unsupported_expr.1",
        site: "lowering/mod.rs::try_lower_native_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_native_call.unsupported_expr.2",
        site: "lowering/mod.rs::try_lower_native_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_native_call.unsupported_expr.3",
        site: "lowering/mod.rs::try_lower_native_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_fn_call.unsupported_expr.1",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_fn_call.unsupported_expr.2",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_fn_call.unknown_stdlib_method.1",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_fn_call.unknown_stdlib_method.2",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_fn_call.unknown_stdlib_method.3",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_fn_call.unsupported_expr.3",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_fn_call.unsupported_expr.4",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_fn_call.unknown_stdlib_method.4",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_fn_call.unknown_stdlib_method.5",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_fn_call.unknown_stdlib_method.6",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_fn_call.split_empty_separator",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "String.split separator is not a provably non-empty string literal; \
                 the tree-walk oracle errors on an empty separator (never a value), so \
                 only a non-empty literal separator lowers byte-equal four-way (Wave R15)",
    },
    LedgerEntry {
        id: "lower_fn_call.unsupported_expr.5",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_fn_call.unsupported_expr.6",
        site: "lowering/mod.rs::lower_fn_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_list_index_typed.unsupported_expr",
        site: "lowering/mod.rs::lower_list_index_typed",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_list_string_index.unsupported_expr",
        site: "lowering/mod.rs::lower_list_string_index",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_string_index.unsupported_expr.1",
        site: "lowering/mod.rs::lower_dict_string_index",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_string_index.unsupported_expr.2",
        site: "lowering/mod.rs::lower_dict_string_index",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_string_index.unsupported_expr.3",
        site: "lowering/mod.rs::lower_dict_string_index",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "expect_int_top.unsupported_expr.1",
        site: "lowering/mod.rs::expect_int_top",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "expect_int_top.unsupported_expr.2",
        site: "lowering/mod.rs::expect_int_top",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_stdlib_arg.unknown_stdlib_method",
        site: "lowering/mod.rs::lower_stdlib_arg",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "lower_stdlib_arg.unsupported_expr.1",
        site: "lowering/mod.rs::lower_stdlib_arg",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_stdlib_arg.unsupported_expr.2",
        site: "lowering/mod.rs::lower_stdlib_arg",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_stdlib_arg.unsupported_expr.3",
        site: "lowering/mod.rs::lower_stdlib_arg",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_method_receiver.unsupported_expr.1",
        site: "lowering/mod.rs::lower_method_receiver",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_method_receiver.unsupported_expr.2",
        site: "lowering/mod.rs::lower_method_receiver",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "finish_schema_method_call.unknown_stdlib_method",
        site: "lowering/mod.rs::finish_schema_method_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "finish_schema_method_call.unsupported_expr.1",
        site: "lowering/mod.rs::finish_schema_method_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "finish_schema_method_call.stdlib_arg_type_mismatch.1",
        site: "lowering/mod.rs::finish_schema_method_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "stdlib call argument type disagrees with the declared signature",
    },
    LedgerEntry {
        id: "finish_schema_method_call.unsupported_expr.2",
        site: "lowering/mod.rs::finish_schema_method_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "finish_schema_method_call.unsupported_expr.3",
        site: "lowering/mod.rs::finish_schema_method_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "finish_schema_method_call.stdlib_arg_type_mismatch.2",
        site: "lowering/mod.rs::finish_schema_method_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "stdlib call argument type disagrees with the declared signature",
    },
    LedgerEntry {
        id: "check_stdlib_arg.unknown_stdlib_method",
        site: "lowering/mod.rs::check_stdlib_arg",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "check_stdlib_arg.stdlib_arg_type_mismatch",
        site: "lowering/mod.rs::check_stdlib_arg",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "stdlib call argument type disagrees with the declared signature",
    },
    LedgerEntry {
        id: "lower_where.unsupported_expr.1",
        site: "lowering/mod.rs::lower_where",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_where.unsupported_expr.2",
        site: "lowering/mod.rs::lower_where",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_where.unsupported_expr.3",
        site: "lowering/mod.rs::lower_where",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_where.unsupported_expr.4",
        site: "lowering/mod.rs::lower_where",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_where.unsupported_expr.5",
        site: "lowering/mod.rs::lower_where",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_where.unsupported_expr.6",
        site: "lowering/mod.rs::lower_where",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.1",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.2",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.3",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.4",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.5",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.6",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.7",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.8",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.9",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.10",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.11",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.12",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.13",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_binary.unsupported_operator.14",
        site: "lowering/mod.rs::lower_binary",
        category: Category::Operator,
        status: Status::Capped,
        corpus: "",
        reason: "operator has no IR Op lowering on these operand types",
    },
    LedgerEntry {
        id: "lower_ternary.unsupported_expr",
        site: "lowering/mod.rs::lower_ternary",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_ternary.if_condition_not_bool",
        site: "lowering/mod.rs::lower_ternary",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "ternary condition lowered to a non-Bool IR type",
    },
    LedgerEntry {
        id: "lower_ternary.if_branch_type_mismatch",
        site: "lowering/mod.rs::lower_ternary",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "ternary arms lowered to incompatible IR types",
    },
    LedgerEntry {
        id: "lower_ternary_as_type.unsupported_expr",
        site: "lowering/mod.rs::lower_ternary_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "target-typed ternary condition produced no IR value",
    },
    LedgerEntry {
        id: "lower_ternary_as_type.if_condition_not_bool",
        site: "lowering/mod.rs::lower_ternary_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "target-typed ternary condition lowered to a non-Bool IR type",
    },
    LedgerEntry {
        id: "lower_ternary_as_type.if_branch_type_mismatch",
        site: "lowering/mod.rs::lower_ternary_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "target-typed ternary arms lowered to incompatible IR types",
    },
    LedgerEntry {
        id: "lower_ternary_as_type.unsupported_expr.branch_type",
        site: "lowering/mod.rs::lower_ternary_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "target-typed ternary branch produced a different IR type than the target",
    },
    LedgerEntry {
        id: "lower_branch.unsupported_expr",
        site: "lowering/mod.rs::lower_branch",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_branch_as_type.unsupported_expr",
        site: "lowering/mod.rs::lower_branch_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "target-typed ternary branch produced an invalid stack shape",
    },
    LedgerEntry {
        id: "canonical_schema_from_def.unsupported_expr",
        site: "lowering/mod.rs::canonical_schema_from_def",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "canonical_schema_from_def.cyclic_field_dependency",
        site: "lowering/mod.rs::canonical_schema_from_def",
        category: Category::SchemaMerge,
        status: Status::Capped,
        corpus: "",
        reason: "schema field defaults form a dependency cycle",
    },
    LedgerEntry {
        id: "canonical_schema_from_def.unsupported_field_type",
        site: "lowering/mod.rs::canonical_schema_from_def",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "canonical_type_repr.unsupported_field_type.1",
        site: "lowering/mod.rs::canonical_type_repr",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "canonical_type_repr.unsupported_field_type.2",
        site: "lowering/mod.rs::canonical_type_repr",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "canonical_type_repr.unsupported_field_type.3",
        site: "lowering/mod.rs::canonical_type_repr",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "topo_order_fields.missing_field_no_default",
        site: "lowering/mod.rs::topo_order_fields",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "branded dict omits a field that has no schema-side default",
    },
    LedgerEntry {
        id: "topo_order_fields.cyclic_field_dependency",
        site: "lowering/mod.rs::topo_order_fields",
        category: Category::SchemaMerge,
        status: Status::Capped,
        corpus: "",
        reason: "schema field defaults form a dependency cycle",
    },
    LedgerEntry {
        id: "check_field_default_refs_resolvable.unknown_field_reference_in_default",
        site: "lowering/mod.rs::check_field_default_refs_resolvable",
        category: Category::SchemaMerge,
        status: Status::Capped,
        corpus: "",
        reason: "a field default references a non-existent sibling field",
    },
    LedgerEntry {
        id: "lower_dict_into_record.unknown_schema_brand",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::SchemaMerge,
        status: Status::Capped,
        corpus: "",
        reason: "dict literal names a schema brand not present in the analyzed tree",
    },
    LedgerEntry {
        id: "spread_source_schema.non_variable",
        site: "lowering/mod.rs::spread_source_schema",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "dict spread source is not a schema-typed identifier; not statically resolvable",
    },
    LedgerEntry {
        id: "spread_source_schema.non_string_head",
        site: "lowering/mod.rs::spread_source_schema",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "dict spread source root is not a bare identifier",
    },
    LedgerEntry {
        id: "spread_source_schema.non_string_segment",
        site: "lowering/mod.rs::spread_source_schema",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "dict spread source path has an index/dynamic segment, not a plain schema field",
    },
    LedgerEntry {
        id: "spread_source_schema.not_a_schema",
        site: "lowering/mod.rs::spread_source_schema",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "dict spread source does not resolve to a statically-known schema value",
    },
    LedgerEntry {
        id: "lower_dict_into_record.duplicate_spread_field",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "two contributors (spread + explicit, or two spreads) supply the same field; \
                 relon rejects duplicates rather than letting a later key override",
    },
    LedgerEntry {
        id: "lower_dict_into_record.duplicate_field",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "the same explicit field appears twice in a branded dict literal",
    },
    LedgerEntry {
        id: "lower_dict_into_record.unsupported_expr.1",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_into_record.unsupported_field_type.1",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_dict_into_record.unsupported_expr.2",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_into_record.unsupported_field_type.2",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_dict_into_record.unsupported_field_type.3",
        site: "lowering/mod.rs::lower_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "field decorator on a branded `-> Schema` field not yet desugared (anon-Dict only)",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_expr.1",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_field_type.1",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_field_type.2",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_expr.2",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_field_type.3",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_dict_default.missing_field_no_default",
        site: "lowering/mod.rs::lower_dict_default",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "branded dict omits a field that has no schema-side default",
    },
    LedgerEntry {
        id: "lower_reference.positional_base",
        site: "lowering/mod.rs::lower_reference",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "positional/runtime/grandparent ref bases (&uncle/&prev/&next/&index/&this) \
                 need loop-carried or cross-dict state the compiled entry body lacks",
    },
    LedgerEntry {
        id: "lower_reference.unsupported_path_shape",
        site: "lowering/mod.rs::lower_reference",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "only a single static String segment lowers; dynamic-key or multi-segment \
                 (&sibling.x.y) reference paths cap",
    },
    LedgerEntry {
        id: "lower_reference.unresolved_field",
        site: "lowering/mod.rs::lower_reference",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "the referenced name binds to no host-visible sibling let (a `#internal` \
                 sibling, dropped from the compiled plan, or a non-field name); forward + \
                 backward `&sibling`/`&root` scalar + List<scalar> refs are covered in \
                 SUPPORTED_SURFACE",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.1",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.2",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unresolved_variable.1",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "identifier resolves to no in-scope binding at lowering time",
    },
    LedgerEntry {
        id: "lower_variable.unresolved_variable.2",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "identifier resolves to no in-scope binding at lowering time",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.3",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.4",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.5",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.6",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.7",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.8",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_field_type",
        site: "lowering/mod.rs::lower_variable",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "schema/dict field type has no canonical layout in this position",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.9",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.10",
        site: "lowering/mod.rs::lower_variable",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "method_signature_ir_types.unsupported_type_in_main.1",
        site: "lowering/mod.rs::method_signature_ir_types",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "method_signature_ir_types.unsupported_type_in_main.2",
        site: "lowering/mod.rs::method_signature_ir_types",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "method_signature_ir_types.unsupported_type_in_main.3",
        site: "lowering/mod.rs::method_signature_ir_types",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "lower_one_method.unsupported_expr.1",
        site: "lowering/mod.rs::lower_one_method",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_one_method.unsupported_expr.2",
        site: "lowering/mod.rs::lower_one_method",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_one_method.unsupported_type_in_main",
        site: "lowering/mod.rs::lower_one_method",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "#main param/return type outside the buffer-protocol decode envelope",
    },
    LedgerEntry {
        id: "try_lower_materialized_list_reduce.unresolved_variable",
        site: "lowering/peephole.rs::try_lower_materialized_list_reduce",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "identifier resolves to no in-scope binding at lowering time",
    },
    LedgerEntry {
        id: "try_lower_materialized_list_reduce.unsupported_expr.1",
        site: "lowering/peephole.rs::try_lower_materialized_list_reduce",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_materialized_list_reduce.unsupported_expr.2",
        site: "lowering/peephole.rs::try_lower_materialized_list_reduce",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_materialized_list_reduce.unsupported_expr.3",
        site: "lowering/peephole.rs::try_lower_materialized_list_reduce",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "combine_operator_to_op.unsupported_expr",
        site: "lowering/peephole.rs::combine_operator_to_op",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_scalar_math.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::try_lower_scalar_math",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled scalar-math (abs/floor/ceil/round/sqrt) slot missing from registry",
    },
    LedgerEntry {
        id: "try_lower_scalar_math.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::try_lower_scalar_math",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled scalar-math (abs/floor/ceil/round/sqrt) metadata missing from registry",
    },
    LedgerEntry {
        id: "try_lower_predicate_math.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::try_lower_predicate_math",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled multiple_of/in_range slot missing from registry",
    },
    LedgerEntry {
        id: "try_lower_predicate_math.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::try_lower_predicate_math",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled multiple_of/in_range metadata missing from registry",
    },
    LedgerEntry {
        id: "try_lower_size_in_range.string_charcount_capped",
        site: "lowering/peephole.rs::try_lower_size_in_range",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "size_in_range on a String counts Unicode code points (chars().count()); needs the \
                 UTF-8 decode seam LLVM-native / wasm do not lower (same seam as upper/title/nfd). \
                 Capped loudly so the String never routes into the byte-length list body",
    },
    LedgerEntry {
        id: "try_lower_size_in_range.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::try_lower_size_in_range",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled size_in_range/dict_size_in_range slot missing from registry",
    },
    LedgerEntry {
        id: "try_lower_size_in_range.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::try_lower_size_in_range",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled size_in_range/dict_size_in_range metadata missing from registry",
    },
    LedgerEntry {
        id: "try_lower_list_filter.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::try_lower_list_filter",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "try_lower_list_filter.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::try_lower_list_filter",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "try_lower_list_filter.unsupported_expr",
        site: "lowering/peephole.rs::try_lower_list_filter",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "try_lower_list_unique.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::try_lower_list_unique",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_int_unique/list_float_unique slot missing from registry",
    },
    LedgerEntry {
        id: "try_lower_list_unique.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::try_lower_list_unique",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_int_unique/list_float_unique metadata missing from registry",
    },
    LedgerEntry {
        id: "emit_list_int_hof_call.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::emit_list_int_hof_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_int_map/filter slot missing from registry",
    },
    LedgerEntry {
        id: "emit_list_int_hof_call.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::emit_list_int_hof_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_int_map/filter metadata missing from registry",
    },
    LedgerEntry {
        id: "emit_list_int_hof_call.unsupported_expr",
        site: "lowering/peephole.rs::emit_list_int_hof_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: list_int_map/filter closure arg signature missing",
    },
    LedgerEntry {
        id: "emit_list_int_fold_call.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::emit_list_int_fold_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_int_fold slot missing from registry",
    },
    LedgerEntry {
        id: "emit_list_int_fold_call.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::emit_list_int_fold_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_int_fold metadata missing from registry",
    },
    LedgerEntry {
        id: "emit_list_int_fold_call.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_list_int_fold_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "_list_reduce init expression is not Int-typed; only Int folds are lowered",
    },
    LedgerEntry {
        id: "emit_list_int_fold_call.unsupported_expr.2",
        site: "lowering/peephole.rs::emit_list_int_fold_call",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: list_int_fold closure arg signature missing",
    },
    LedgerEntry {
        id: "lower_comprehension.unsupported_expr.1",
        site: "lowering/mod.rs::lower_comprehension",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "comprehension iterable does not lower to a List<Int>/Float/String in the AOT envelope (e.g. List<Bool>)",
    },
    LedgerEntry {
        id: "lower_comprehension.unsupported_expr.2",
        site: "lowering/mod.rs::lower_comprehension",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "filtered List<String> comprehension (no four-way String->Bool predicate body), or internal guard: list_*_map/filter closure arg signature missing",
    },
    LedgerEntry {
        id: "lower_comprehension.unsupported_expr.3",
        site: "lowering/mod.rs::lower_comprehension",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "comprehension element type matches no bundled map body for the source (no cross-type list_*_map_to_* exists)",
    },
    LedgerEntry {
        id: "lower_comprehension.unknown_stdlib_method.1",
        site: "lowering/mod.rs::lower_comprehension",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_*_map/filter slot missing from registry",
    },
    LedgerEntry {
        id: "lower_comprehension.unknown_stdlib_method.2",
        site: "lowering/mod.rs::lower_comprehension",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "internal guard: bundled list_*_map/filter metadata missing from registry",
    },
    LedgerEntry {
        id: "try_lower_list_sum_value.unknown_stdlib_method.1",
        site: "lowering/peephole.rs::try_lower_list_sum_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "try_lower_list_sum_value.unknown_stdlib_method.2",
        site: "lowering/peephole.rs::try_lower_list_sum_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "method/function name + arity matches no stdlib signature",
    },
    LedgerEntry {
        id: "emit_list_float_literal_materialize.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_list_float_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_float_literal_materialize.unsupported_expr.2",
        site: "lowering/peephole.rs::emit_list_float_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_float_literal_materialize.unsupported_expr.3",
        site: "lowering/peephole.rs::emit_list_float_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_float_literal_materialize.unsupported_expr.4",
        site: "lowering/peephole.rs::emit_list_float_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_int_literal_materialize.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_list_int_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_int_literal_materialize.unsupported_expr.2",
        site: "lowering/peephole.rs::emit_list_int_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_int_literal_materialize.unsupported_expr.3",
        site: "lowering/peephole.rs::emit_list_int_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_int_literal_materialize.unsupported_expr.4",
        site: "lowering/peephole.rs::emit_list_int_literal_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "probe_expr_ir_ty.unsupported_expr",
        site: "lowering/peephole.rs::probe_expr_ir_ty",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "flatten_list_spread.unsupported_spread_source",
        site: "lowering/peephole.rs::flatten_list_spread",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "r12_list_spread_computed_elem_capped",
        reason: "list spread mixes a runtime (non-literal) source with a non-scalar-literal \
                 surrounding element (`[n, ...a]`) or with a list-literal source \
                 (`[...[1], ...a]`); the runtime materialiser admits static Int/Float \
                 literal scalars around any number of runtime sources, everything else \
                 falls back to the static flatten, which cannot resolve a runtime source",
    },
    LedgerEntry {
        id: "emit_list_spread_runtime_materialize.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_list_spread_runtime_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "runtime list spread source produced no value during lowering",
    },
    LedgerEntry {
        id: "emit_list_spread_runtime_materialize.unsupported_source_ty",
        site: "lowering/peephole.rs::emit_list_spread_runtime_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "runtime list spread source is not a scalar List<Int>/List<Float>; \
                 List<String>/List<Schema>/List<List> sources need the pointer-array \
                 materialiser, not the scalar memory.copy path",
    },
    LedgerEntry {
        id: "emit_list_spread_runtime_materialize.mixed_source_ty",
        site: "lowering/peephole.rs::emit_list_spread_runtime_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "runtime list spread sources disagree on the scalar element shape \
                 (List<Int> vs List<Float>); the analyzer enforces a homogeneous element \
                 type, so this is a defensive lowering cap",
    },
    LedgerEntry {
        id: "emit_list_spread_runtime_materialize.unsupported_expr.static_count_overflow",
        site: "lowering/peephole.rs::emit_list_spread_runtime_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "list spread static scalar element count overflows i32",
    },
    LedgerEntry {
        id: "emit_scalar_element_value_store.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_scalar_element_value_store",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "list spread scalar element produced no value during lowering",
    },
    LedgerEntry {
        id: "emit_scalar_element_value_store.unsupported_expr.2",
        site: "lowering/peephole.rs::emit_scalar_element_value_store",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "list spread Float scalar element lowered to a non-Float/Int type",
    },
    LedgerEntry {
        id: "emit_scalar_element_value_store.unsupported_expr.3",
        site: "lowering/peephole.rs::emit_scalar_element_value_store",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "list spread Int scalar element lowered to a non-Int type",
    },
    LedgerEntry {
        id: "emit_list_value_materialize.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_list_value_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_value_materialize.unsupported_expr.2",
        site: "lowering/peephole.rs::emit_list_value_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_value_materialize.unsupported_expr.3",
        site: "lowering/peephole.rs::emit_list_value_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_list_value_materialize.unsupported_expr.4",
        site: "lowering/peephole.rs::emit_list_value_materialize",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.1",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.2",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.3",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.4",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.5",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.6",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.7",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "emit_range_pipeline_loop.unsupported_expr.8",
        site: "lowering/peephole.rs::emit_range_pipeline_loop",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "resolve_capture.unsupported_closure_capture",
        site: "lowering/closure.rs::resolve_capture",
        category: Category::Closure,
        status: Status::Capped,
        corpus: "",
        reason: "capture type has no static byte layout in the captures struct",
    },
    LedgerEntry {
        id: "lower_closure_as_value.unsupported_expr.1",
        site: "lowering/closure.rs::lower_closure_as_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_closure_as_value.unsupported_expr.2",
        site: "lowering/closure.rs::lower_closure_as_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_closure_as_value.unsupported_expr.3",
        site: "lowering/closure.rs::lower_closure_as_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_closure_as_value.stdlib_arg_type_mismatch",
        site: "lowering/closure.rs::lower_closure_as_value",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "stdlib call argument type disagrees with the declared signature",
    },
    LedgerEntry {
        id: "lower_match.unsupported_expr.1",
        site: "lowering/mod.rs::try_lower_runtime_enum_match",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "runtime-enum `match` arm pattern is not a variant of the scrutinee enum \
                 (or an unsupported pattern shape). The dynamic runtime-`#brand` dispatch \
                 case that this id also defended is now rejected up-front by the analyzer \
                 (Diagnostic::DynamicBrandDispatchMatch), so the lower_match arm-undecidable \
                 site is a defensive cap unreachable for analyzed programs",
    },
    LedgerEntry {
        id: "lower_match.empty_enum_match",
        site: "lowering/mod.rs::try_lower_runtime_enum_match",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "runtime-enum `match` with zero arms / zero lowerable arms — a degenerate, \
                 unreachable-by-the-analyzer shape with no body to dispatch to; capped",
    },
    LedgerEntry {
        id: "lower_match.no_match_trap_result_ty",
        site: "lowering/mod.rs::try_lower_runtime_enum_match",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "non-exhaustive runtime-enum `match` whose result type has no scalar \
                 placeholder const for the no-match trap arm (e.g. a List/Dict-typed body); \
                 the TrapKind::NoMatch trap needs a typed dead value and only scalars/String \
                 have one today — capped for the exotic result type rather than miscompile",
    },
    LedgerEntry {
        id: "lower_match.unsupported_expr.3",
        site: "lowering/mod.rs::lower_match",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "match scrutinee produced no IR value during real lowering; defensive cap",
    },
    LedgerEntry {
        id: "canonical_schema_from_def.unsupported_tuple_element_type",
        site: "lowering/mod.rs::canonical_schema_from_def",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "tuple value shape does not match the canonical tuple schema",
    },
    LedgerEntry {
        id: "canonical_type_repr.unsupported_field_type.generics",
        site: "lowering/mod.rs::canonical_type_repr",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "source type cannot be canonicalized into the compiled schema/type representation",
    },
    LedgerEntry {
        id: "canonical_type_repr.unsupported_field_type.reserved",
        site: "lowering/mod.rs::canonical_type_repr",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "source type cannot be canonicalized into the compiled schema/type representation",
    },
    LedgerEntry {
        id: "canonical_type_repr.unsupported_field_type.tuple",
        site: "lowering/mod.rs::canonical_type_repr",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "tuple value shape does not match the canonical tuple schema",
    },
    LedgerEntry {
        id: "classify_anon_dict_enum_list_field.unsupported_field_type.1",
        site: "lowering/mod.rs::classify_anon_dict_enum_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> field source is not in the supported pointer-list construction path",
    },
    LedgerEntry {
        id: "classify_anon_dict_enum_list_field.unsupported_field_type.2",
        site: "lowering/mod.rs::classify_anon_dict_enum_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> field source is not in the supported pointer-list construction path",
    },
    LedgerEntry {
        id: "classify_anon_dict_enum_list_field.unsupported_field_type.3",
        site: "lowering/mod.rs::classify_anon_dict_enum_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> field source is not in the supported pointer-list construction path",
    },
    LedgerEntry {
        id: "classify_anon_dict_enum_list_field.unsupported_field_type.4",
        site: "lowering/mod.rs::classify_anon_dict_enum_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> field source is not in the supported pointer-list construction path",
    },
    LedgerEntry {
        id: "classify_anon_dict_variant_list_field.unsupported_field_type.1",
        site: "lowering/mod.rs::classify_anon_dict_variant_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "anon-Dict variant-list field element is not an Option/Result variant constructor",
    },
    LedgerEntry {
        id: "classify_anon_dict_variant_list_field.unsupported_field_type.2",
        site: "lowering/mod.rs::classify_anon_dict_variant_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "heterogeneous anon-Dict variant-list field (mixed Option/Result enum head)",
    },
    LedgerEntry {
        id: "classify_anon_dict_variant_list_field.unsupported_field_type.3",
        site: "lowering/mod.rs::classify_anon_dict_variant_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "anon-Dict variant-list field names an unknown Option/Result variant",
    },
    LedgerEntry {
        id: "classify_anon_dict_variant_list_field.unsupported_field_type.4",
        site: "lowering/mod.rs::classify_anon_dict_variant_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "anon-Dict variant-list payload is not a scalar literal or scalar #main parameter, so the element type cannot be proven",
    },
    LedgerEntry {
        id: "classify_anon_dict_variant_list_field.unsupported_field_type.5",
        site: "lowering/mod.rs::classify_anon_dict_variant_list_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "heterogeneous anon-Dict variant-list payload scalar type across elements",
    },
    LedgerEntry {
        id: "emit_standard_variant_record.empty_payload_stack",
        site: "lowering/mod.rs::emit_standard_variant_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "emit_standard_variant_record.missing_payload",
        site: "lowering/mod.rs::emit_standard_variant_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "emit_standard_variant_record.payload_type_mismatch",
        site: "lowering/mod.rs::emit_standard_variant_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "emit_standard_variant_record.unexpected_payload",
        site: "lowering/mod.rs::emit_standard_variant_record",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_expr.variant_list_stack_empty",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> field source is not in the supported pointer-list construction path",
    },
    LedgerEntry {
        id: "lower_dict_field_value.unsupported_field_type.variant_list_stack",
        site: "lowering/mod.rs::lower_dict_field_value",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> field source is not in the supported pointer-list construction path",
    },
    LedgerEntry {
        id: "lower_match.enum_pattern_binding_stack",
        site: "lowering/mod.rs::lower_match enum payload pattern",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum match payload pattern is malformed or asks for payload data that is not present",
    },
    LedgerEntry {
        id: "lower_match.enum_pattern_duplicate_binding",
        site: "lowering/mod.rs::lower_match enum payload pattern",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum match payload pattern is malformed or asks for payload data that is not present",
    },
    LedgerEntry {
        id: "lower_match.enum_pattern_unit_payload",
        site: "lowering/mod.rs::lower_match enum payload pattern",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum match payload pattern is malformed or asks for payload data that is not present",
    },
    LedgerEntry {
        id: "lower_match.enum_pattern_unknown_payload",
        site: "lowering/mod.rs::lower_match enum payload pattern",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum match payload pattern is malformed or asks for payload data that is not present",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.missing_field",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.unsupported_expr.1",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.unsupported_expr.2",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.unsupported_field_type.1",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.unsupported_field_type.2",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.unsupported_field_type.3",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_plain_dict_into_record.unsupported_field_type.4",
        site: "lowering/mod.rs::lower_plain_dict_into_record",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "synthetic payload record literal does not match the canonical payload schema",
    },
    LedgerEntry {
        id: "lower_prelude_variant_call_as_type.arity_mismatch",
        site: "lowering/mod.rs::lower_prelude_variant_call_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_schema_value_as_absolute_pointer.arity_mismatch",
        site: "lowering/mod.rs::lower_schema_value_as_absolute_pointer",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_tuple_field.arity_mismatch",
        site: "lowering/mod.rs::lower_tuple_field",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "tuple value shape does not match the canonical tuple schema",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_payload_missing_layout",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum payload field/index access is outside the lowered payload layout",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_payload_path",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum payload field/index access is outside the lowered payload layout",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_payload_segment",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum payload field/index access is outside the lowered payload layout",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_payload_stack",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum payload field/index access is outside the lowered payload layout",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_payload_stack_type",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "enum payload field/index access is outside the lowered payload layout",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_payload_unknown_field",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "enum payload field/index access is outside the lowered payload layout",
    },
    LedgerEntry {
        id: "lower_variable.unsupported_expr.enum_unit_payload_access",
        site: "lowering/mod.rs::lower_variable enum payload access",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "expression shape has no IR/codegen lowering path yet",
    },
    LedgerEntry {
        id: "lower_variant_ctor_as_type.body_not_dict",
        site: "lowering/mod.rs::lower_variant_ctor_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_variant_ctor_as_type.duplicate_payload",
        site: "lowering/mod.rs::lower_variant_ctor_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_variant_ctor_as_type.missing_payload",
        site: "lowering/mod.rs::lower_variant_ctor_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_variant_ctor_as_type.non_string_key",
        site: "lowering/mod.rs::lower_variant_ctor_as_type",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_variant_ctor_as_type.unexpected_field",
        site: "lowering/mod.rs::lower_variant_ctor_as_type",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_variant_ctor_as_type.unit_variant_has_fields",
        site: "lowering/mod.rs::lower_variant_ctor_as_type",
        category: Category::FieldType,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "lower_variant_record_list_literal.element_type_mismatch",
        site: "lowering/mod.rs::lower_variant_record_list_literal",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> literal element cannot be converted to a variant-record pointer",
    },
    LedgerEntry {
        id: "lower_variant_record_list_literal.empty_element_stack",
        site: "lowering/mod.rs::lower_variant_record_list_literal",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> literal element cannot be converted to a variant-record pointer",
    },
    LedgerEntry {
        id: "lower_variant_record_list_literal.length_overflow",
        site: "lowering/mod.rs::lower_variant_record_list_literal",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "List<Option/Result/Enum> literal element cannot be converted to a variant-record pointer",
    },
    LedgerEntry {
        id: "payload_slot_layout_for_lowering.unsupported_type",
        site: "lowering/mod.rs::payload_slot_layout_for_lowering",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "variant payload type has no compiled binary layout yet",
    },
    LedgerEntry {
        id: "standard_variant_shape.not_variant_type",
        site: "lowering/mod.rs::standard_variant_shape",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "standard_variant_shape.option_enum_mismatch",
        site: "lowering/mod.rs::standard_variant_shape",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "standard_variant_shape.option_variant_mismatch",
        site: "lowering/mod.rs::standard_variant_shape",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "standard_variant_shape.result_enum_mismatch",
        site: "lowering/mod.rs::standard_variant_shape",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "standard_variant_shape.result_variant_mismatch",
        site: "lowering/mod.rs::standard_variant_shape",
        category: Category::ExprShape,
        status: Status::Capped,
        corpus: "",
        reason: "variant constructor payload shape does not match the target enum variant",
    },
    LedgerEntry {
        id: "type_graph_alignment_for_lowering.unsupported_type",
        site: "lowering/mod.rs::type_graph_alignment_for_lowering",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "variant payload type has no compiled binary layout yet",
    },
    LedgerEntry {
        id: "variant_record_alignment_for_lowering.unsupported_type",
        site: "lowering/mod.rs::variant_record_alignment_for_lowering",
        category: Category::MainSignatureType,
        status: Status::Capped,
        corpus: "",
        reason: "variant payload type has no compiled binary layout yet",
    },
];

/// One row per declared-supported construct family. Unlike [`LEDGER`]
/// (which is keyed by cap *site* and is therefore all-Capped), this table
/// is keyed by *construct* and is all-[`Status::Covered`]: every row names
/// a real corpus case that lowers cleanly (no `cap!` fires) and runs the
/// compiled backend in the differential.
///
/// The Wave R6 `no_fallback_over_supported_surface` proof iterates this
/// table, looks each `corpus` name up in [`crate::corpus::all_cases`],
/// runs it through `Backend::Auto`, and asserts the compiled backend
/// handled it — i.e. `auto` did NOT take the unsupported-shape capability
/// fallback to the tree-walk interpreter. See that test for the
/// trivial-scalar-`#main` perf-route carve-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceEntry {
    /// Construct family / sub-shape this row attests is supported.
    pub construct: &'static str,
    /// Wave that landed the lowering (R1–R9, or "base" for the
    /// arith/cmp/control-flow envelope that predates the R series).
    pub wave: &'static str,
    /// CorpusCase name in [`crate::corpus::all_cases`] that exercises
    /// the construct and lowers four-way (or `TW_CR` with the wasm /
    /// llvm-native legs proven in a dedicated codegen test — noted in
    /// `proof`). The no-fallback proof drives this exact case.
    pub corpus: &'static str,
    /// Always [`Status::Covered`] — the table is the supported surface.
    pub status: Status,
    /// How the construct is proven across backends. Honest about which
    /// legs run inline vs in a dedicated test.
    pub proof: &'static str,
}

/// The declared-supported surface. Every row is `Covered`; the
/// no-fallback proof gates `auto` against this list.
pub const SUPPORTED_SURFACE: &[SurfaceEntry] = &[
    // ---- base envelope (pre-R): arith / cmp / control flow / where ----
    SurfaceEntry {
        construct: "Int arithmetic (+ - * / %)",
        wave: "base",
        corpus: "arith_chain",
        status: Status::Covered,
        proof: "four-way (tree-walk + cranelift + wasm + llvm-native), FULL_SUPPORT",
    },
    SurfaceEntry {
        construct: "Int comparison (== != < > <= >=)",
        wave: "base",
        corpus: "cmp_lt",
        status: Status::Covered,
        proof: "four-way, FULL_SUPPORT",
    },
    SurfaceEntry {
        construct: "ternary control flow (cond ? a : b), nested",
        wave: "base",
        corpus: "if_nested",
        status: Status::Covered,
        proof: "four-way, FULL_SUPPORT",
    },
    SurfaceEntry {
        construct: "where-binding (postfix let)",
        wave: "base",
        corpus: "let_then_add",
        status: Status::Covered,
        proof: "four-way, FULL_SUPPORT",
    },
    SurfaceEntry {
        construct: "arithmetic trap parity (div-by-zero / overflow)",
        wave: "base",
        corpus: "arith_div_by_zero_traps",
        status: Status::Covered,
        proof: "tree-walk + cranelift + bytecode agree on the trap (trace synth wraps)",
    },
    // ---- simple stdlib (scalar) ----
    SurfaceEntry {
        construct: "scalar stdlib (abs/min/max/length/is_empty)",
        wave: "base",
        corpus: "stdlib_abs_neg",
        status: Status::Covered,
        proof: "four-way, FULL_SUPPORT",
    },
    // ---- Wave R1: contextual-typed closures via list HOFs ----
    SurfaceEntry {
        construct: "contextual-typed closure as HOF arg (range.map)",
        wave: "R1/R3",
        corpus: "r3_range_map",
        status: Status::Covered,
        proof: "tree-walk + cranelift inline; closure dispatched via Op::CallClosure",
    },
    SurfaceEntry {
        construct: "comprehension desugar onto list HOFs",
        wave: "R1/R3",
        corpus: "r3_comprehension_if",
        status: Status::Covered,
        proof: "tree-walk + cranelift inline (desugars to map/filter)",
    },
    SurfaceEntry {
        construct: "comprehension over List<Float> source (map / guard)",
        wave: "R3b",
        corpus: "r3b_comprehension_float_if",
        status: Status::Covered,
        proof: "tree-walk + cranelift; four-way in inplace_return_four_way::comprehension_float_*",
    },
    SurfaceEntry {
        construct: "comprehension Int->Float element map",
        wave: "R3b",
        corpus: "r3b_comprehension_int_to_float",
        status: Status::Covered,
        proof: "tree-walk + cranelift; four-way in inplace_return_four_way::comprehension_int_to_float",
    },
    SurfaceEntry {
        construct: "comprehension over List<String> source (map)",
        wave: "R3c",
        corpus: "r3c_comprehension_string",
        status: Status::Covered,
        proof: "tree-walk + cranelift; four-way in inplace_return_four_way::comprehension_string_source_map",
    },
    SurfaceEntry {
        construct: "comprehension Int->String element map",
        wave: "R3c",
        corpus: "r3c_comprehension_int_to_string",
        status: Status::Covered,
        proof: "tree-walk + cranelift; four-way in inplace_return_four_way::comprehension_int_to_string",
    },
    // ---- Wave R2: pipe + f-string ----
    SurfaceEntry {
        construct: "pipe operator (range(n) | list.sum)",
        wave: "R2",
        corpus: "pipe_range_into_list_sum",
        status: Status::Covered,
        proof: "tree-walk + cranelift + bytecode (pure desugar to the spelled-out call)",
    },
    SurfaceEntry {
        construct: "f-string interpolation (String + Int parts)",
        wave: "R2",
        corpus: "fstring_mixed_parts",
        status: Status::Covered,
        proof: "tree-walk + cranelift (String return; bytecode/trace exclude String entry)",
    },
    // ---- Wave 1: unified value→String skeleton, Bool leg ----
    SurfaceEntry {
        construct: "Bool value→String render (f-string interpolation, b ? \"true\" : \"false\")",
        wave: "W1",
        corpus: "fstring_bool_interp_true",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR); wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::aot_wasm_parity::fstring_bool_interp_aligns_native_via_wasmtime \
                (both Bool values)",
    },
    SurfaceEntry {
        construct: "String + Bool / Bool + String coercion concat (render-then-StrConcatN)",
        wave: "W1",
        corpus: "string_plus_bool_true",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR); wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::aot_wasm_parity::{string_plus_bool,bool_plus_string}\
                _aligns_native_via_wasmtime (both Bool values, both operand orders)",
    },
    // ---- Wave B: unified value→String skeleton, Float leg ----
    // `Op::FloatToStr` routes every compiled backend through ONE Rust
    // Display byte producer (relon_ir::float_str::format_f64_display —
    // the tree-walk oracle's `Value::Float` `format!` path): cranelift
    // via vtable slot `RelonF64ToStr`, llvm-native via
    // `add_global_mapping(relon_llvm_f64_to_str)`, wasm32 via the same
    // fn `func_wrap`ped as an `env` import. `1.0 → "1"`, `-0.0 → "-0"`,
    // `NaN` / `inf` / `-inf`, 327-char subnormal expansion — equal by
    // construction.
    SurfaceEntry {
        construct: "Float value→String render (f-string interpolation, Op::FloatToStr)",
        wave: "WB",
        corpus: "wave_b_fstring_float_typical",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR, 9-value boundary battery incl. -0.0 / NaN / \
                ±inf / 5e-324 / 1e300); wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::aot_wasm_parity::\
                wave_b_fstring_float_battery_aligns_native_via_wasmtime (same battery)",
    },
    SurfaceEntry {
        construct: "String + Float / Float + String coercion concat (render-then-StrConcatN)",
        wave: "WB",
        corpus: "string_plus_float_typical",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR, both operand orders); wasm + llvm-native legs \
                proven four-way in relon-codegen-llvm::aot_wasm_parity::\
                wave_b_concat_{string_float,float_string}_battery_aligns_native_via_wasmtime \
                (full boundary battery, both operand orders)",
    },
    SurfaceEntry {
        construct: "Float-valued field decorator with String-concat body (@currency(\"USD\") \
                    display: price → currency(price, \"USD\"); concat-coercible param mask \
                    renders the scalar arg via Op::FloatToStr at the call edge)",
        wave: "WB",
        corpus: "wave_b_currency_decorator",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR, display == \"USD 567.34\"); wasm + llvm-native \
                legs proven four-way in relon-codegen-llvm::aot_wasm_parity::\
                wave_b_currency_decorator_aligns_native_via_wasmtime; lowering shape asserted \
                by relon-ir::lowering::tests::anon_dict_float_string_concat_decorator_lowers; \
                explicitly-annotated String params keep rejecting scalar args \
                (relon-ir::lowering::tests::annotated_string_param_rejects_float_arg)",
    },
    // ---- Wave R3: range / map / filter / reduce / comprehension (Int) ----
    SurfaceEntry {
        construct: "range(n) as materialised List<Int>",
        wave: "R3",
        corpus: "r3_range_value",
        status: Status::Covered,
        proof: "tree-walk + cranelift (List<Int> return; wasm/llvm in codegen tests)",
    },
    SurfaceEntry {
        construct: "list filter (range.filter)",
        wave: "R3",
        corpus: "r3_range_filter",
        status: Status::Covered,
        proof: "tree-walk + cranelift inline",
    },
    SurfaceEntry {
        construct: "list reduce (_list_reduce, Int)",
        wave: "R3",
        corpus: "r3_list_reduce_free",
        status: Status::Covered,
        proof: "tree-walk + cranelift + bytecode (Int return)",
    },
    // ---- Wave R3b: List<Float> HOFs + cross-type numeric map ----
    SurfaceEntry {
        construct: "List<Float> map / filter / reduce",
        wave: "R3b",
        corpus: "r3b_float_reduce_sum",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in list_float_hof_four_way",
    },
    SurfaceEntry {
        construct: "cross-type numeric map (Int->Float)",
        wave: "R3b",
        corpus: "r3b_int_to_float_map",
        status: Status::Covered,
        proof: "tree-walk + cranelift (result element type from closure return)",
    },
    // ---- Wave R3c: String-result map family ----
    SurfaceEntry {
        construct: "String-result list map (range.map -> f-string)",
        wave: "R3c",
        corpus: "r3c_range_map_fstring",
        status: Status::Covered,
        proof: "tree-walk + cranelift + llvm-native (three-way in list_string_hof_three_way; \
                wasm List<String> decode pending — capped, not claimed here)",
    },
    // ---- Wave R4: type() static const-fold ----
    SurfaceEntry {
        construct: "type(v) static const-fold (scalar)",
        wave: "R4",
        corpus: "r4_type_int",
        status: Status::Covered,
        proof: "tree-walk + cranelift (folds to constant type-name String)",
    },
    SurfaceEntry {
        construct: "type(v) coarsening (List -> \"List\")",
        wave: "R4",
        corpus: "r4_type_list_coarsen",
        status: Status::Covered,
        proof: "tree-walk + cranelift",
    },
    SurfaceEntry {
        construct: "type(v) arg-eval trap parity",
        wave: "R4",
        corpus: "r4_type_arg_overflow_traps",
        status: Status::Covered,
        proof: "tree-walk + cranelift agree on the overflow trap before the type string",
    },
    // ---- Wave R5: strict-match static arm selection ----
    SurfaceEntry {
        construct: "strict match static arm-fold (builtin scalar arm)",
        wave: "R5",
        corpus: "r5_match_int_arm",
        status: Status::Covered,
        proof: "tree-walk + cranelift (first statically-matching arm wins)",
    },
    SurfaceEntry {
        construct: "strict match selected-arm general body codegen",
        wave: "R5",
        corpus: "r5_match_int_body_arith",
        status: Status::Covered,
        proof: "tree-walk + cranelift + bytecode (Int return; body is real IR, not a const)",
    },
    SurfaceEntry {
        construct: "strict match source-order tie-break + mismatch->wildcard",
        wave: "R5",
        corpus: "r5_match_scalar_mismatch_falls_to_wildcard",
        status: Status::Covered,
        proof: "tree-walk + cranelift (provably-never arm skipped, wildcard wins)",
    },
    SurfaceEntry {
        construct: "strict match no-arm trap (TrapKind::NoMatch -> TypeMismatch)",
        wave: "R6",
        corpus: "r5_match_no_arm_traps",
        status: Status::Covered,
        proof: "tree-walk + cranelift trap-equivalent; wasm/llvm-native surface the same \
                 RuntimeError::TypeMismatch via TrapKind::NoMatch in match_no_arm_four_way",
    },
    // ---- Wave R7: scalar Float math ----
    SurfaceEntry {
        construct: "scalar Float math (abs/floor/ceil/round/sqrt)",
        wave: "R7",
        corpus: "r7_round_ties_even_up",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::r7_*",
    },
    SurfaceEntry {
        construct: "Float math Int-widen (sqrt(Int))",
        wave: "R7",
        corpus: "r7_sqrt_int_widen",
        status: Status::Covered,
        proof: "tree-walk + cranelift",
    },
    // ---- Wave R8: byte-level scalar string ops ----
    SurfaceEntry {
        construct: "scalar string len (byte / unicode)",
        wave: "R8",
        corpus: "r8_len_unicode",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::r8_*",
    },
    SurfaceEntry {
        construct: "scalar string ends_with",
        wave: "R8",
        corpus: "r8_ends_with_true",
        status: Status::Covered,
        proof: "tree-walk + cranelift",
    },
    SurfaceEntry {
        construct: "scalar string replace (grow / overlap / empty-from)",
        wave: "R8",
        corpus: "r8_replace_overlap",
        status: Status::Covered,
        proof: "tree-walk + cranelift",
    },
    // ---- Wave R9: is_uuid validator ----
    SurfaceEntry {
        construct: "is_uuid validator (valid / invalid edges)",
        wave: "R9",
        corpus: "r9_is_uuid_valid",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::r9_*",
    },
    SurfaceEntry {
        construct: "is_uuid rejects bad dash / non-hex / short",
        wave: "R9",
        corpus: "r9_is_uuid_nonhex",
        status: Status::Covered,
        proof: "tree-walk + cranelift",
    },
    // ---- JSON-Schema numeric / size predicates (Int / Float / List
    //      arms four-way; Float-mod & String-charcount arms stay capped) ----
    SurfaceEntry {
        construct: "multiple_of(Int, Int) -> Bool (zero-divisor short-circuits to false)",
        wave: "JS",
        corpus: "js_multiple_of_true",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_multiple_of_*",
    },
    SurfaceEntry {
        construct: "in_range(n, lo, hi) -> Bool (all-f64; Int args widened to f64)",
        wave: "JS",
        corpus: "js_in_range_int_inside",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_in_range_*",
    },
    SurfaceEntry {
        construct: "size_in_range(List<_>, lo, hi) -> Bool (element count from record header)",
        wave: "JS",
        corpus: "js_size_in_range_list_inside",
        status: Status::Covered,
        proof:
            "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_size_in_range_list_*",
    },
    // ---- `trim` / `trim_start` / `trim_end` (Rust `str::trim*`) and
    //      the ASCII-structured validators `is_email` / `is_uri`, lowered
    //      four-way now that the UTF-8 decode seam (R14) is in place ----
    SurfaceEntry {
        construct: "trim/trim_start/trim_end(String) -> String (Unicode White_Space, \
                    char::is_whitespace-exact; forward UTF-8 decode + __is_whitespace + memcpy)",
        wave: "JS",
        corpus: "js_trim_ascii",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_trim_*",
    },
    SurfaceEntry {
        construct: "is_email(String) -> Bool (byte-level ASCII structure: @ split, local \
                    char class + dot rules, domain label grammar; non-ASCII rejected)",
        wave: "JS",
        corpus: "js_is_email_valid",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_is_email_*",
    },
    SurfaceEntry {
        construct: "is_uri(String) -> Bool (scheme ':' non-empty-rest, ASCII scheme grammar; \
                    no UTF-8 decode, no remainder)",
        wave: "JS",
        corpus: "js_is_uri_valid",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_is_uri_*",
    },
    SurfaceEntry {
        construct: "is_iso_date(String) -> Bool (RFC 3339 YYYY-MM-DD: byte-level shape + \
                    integer date arithmetic; leap-year test over Op::Mod(I32) with non-zero \
                    constant divisors)",
        wave: "JS",
        corpus: "js_is_iso_date_valid",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native in aot_wasm_parity::js_is_iso_date_*",
    },
    // ---- Wave R15: `split(String, sep) -> List<String>` ----
    SurfaceEntry {
        construct: "String split on non-empty literal separator (List<String>, \
                    data-dependent count)",
        wave: "R15",
        corpus: "r15_split_basic",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native four-way in \
                relon-codegen-llvm::inplace_return_four_way::r15_split_*",
    },
    SurfaceEntry {
        construct: "String split edges (empty input / leading / trailing / \
                    consecutive / no-match / multi-char / utf-8)",
        wave: "R15",
        corpus: "r15_split_consecutive_empty",
        status: Status::Covered,
        proof: "tree-walk + cranelift; wasm/llvm-native four-way in \
                relon-codegen-llvm::inplace_return_four_way::r15_split_*. \
                Empty separator stays capped (oracle errors, no value to match).",
    },
    // ---- str concat fold (Op::StrConcatN) ----
    SurfaceEntry {
        construct: "String concat-chain fold (StrConcatN)",
        wave: "base",
        corpus: "str_concat_chain_four_way",
        status: Status::Covered,
        proof: "tree-walk + cranelift (4-leaf chain folds to one StrConcatN)",
    },
    // ---- schema-rooted dict return (the supported dict shape) ----
    SurfaceEntry {
        construct: "schema-rooted dict return (Int field)",
        wave: "base",
        corpus: "dict_simple_return",
        status: Status::Covered,
        proof: "tree-walk + cranelift + bytecode (schema-rooted, NOT a Dict param/literal)",
    },
    // ---- Wave T2: tuple return (anonymous positional record) ----
    SurfaceEntry {
        construct: "tuple return of scalars (Tuple<Int, String, Bool>) → JSON array",
        wave: "T2",
        corpus: "tuple_scalar_return",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::tuple_return_four_way)",
    },
    SurfaceEntry {
        construct: "tuple return all-inline scalars (Tuple<Int, Int>) → JSON array",
        wave: "T2",
        corpus: "tuple_int_pair_return",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::tuple_return_four_way)",
    },
    // ---- Wave T3: tuple input + compiled positional access ----
    SurfaceEntry {
        construct: "tuple parameter positional access (`pair.0`) returning a scalar",
        wave: "T3",
        corpus: "tuple_param_index_arith_return",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::tuple_return_four_way)",
    },
    SurfaceEntry {
        construct: "tuple parameter projected through `.N` into a tuple return",
        wave: "T3",
        corpus: "tuple_param_project_tuple_return",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven four-way in \
                relon-codegen-llvm::tuple_return_four_way)",
    },
    // ---- backward static sibling/root field reference ----
    SurfaceEntry {
        construct: "backward `&sibling`/`&root` scalar field reference (anon-Dict return)",
        wave: "R10",
        corpus: "r10_sibling_backward",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r10_sibling_root_backward)",
    },
    // ---- STRICT-mode static type derivation of the above reference ----
    SurfaceEntry {
        construct:
            "strict-mode `&sibling`/`&root` scalar field reference (derived type, no #relaxed)",
        wave: "R10b",
        corpus: "r10b_strict_sibling_chain",
        status: Status::Covered,
        proof: "analyzer-only: relon-analyzer derives the reference type from the backward \
                sibling/root field (infer::infer_reference); lowering unchanged. tree-walk + \
                cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r10b_strict_sibling_root_backward)",
    },
    // ---- forward + mixed static sibling/root field reference ----
    SurfaceEntry {
        construct: "forward/backward `&sibling`/`&root` scalar + List<scalar> field reference \
                    (anon-Dict return, topological emit order)",
        wave: "R13",
        corpus: "r13_forward_ref",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r13_forward_ref)",
    },
    // ---- field decorators on the anon-Dict-return path ----
    SurfaceEntry {
        construct: "field decorator desugar (@deco(args) k: v -> deco(v, args)), stacked",
        wave: "R11",
        corpus: "r11_int_decorator",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r11_field_decorator)",
    },
    SurfaceEntry {
        construct: "String-result field decorator (@deco() k: v where deco's body is a \
                    pure `String + String + ...` concat — closure (param, ret) signature \
                    read as String from the body, value-first-desugared call lowers through \
                    `Op::CallClosure { [String] -> String }` + `Op::StrConcatN`). Wave B \
                    unsealed Float/Int/Bool-valued concat operands via Op::FloatToStr + the \
                    concat-coercible param mask (see the WB decorator row).",
        wave: "R11",
        corpus: "r11_string_result_decorator",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r11_string_result_decorator; the \
                Float-concat decorator lowering is asserted by \
                relon-ir::lowering::tests::anon_dict_float_string_concat_decorator_lowers)",
    },
    // ---- Wave R12-lower: spread (`...x`), the two static forms ----
    SurfaceEntry {
        construct: "list spread `[...a, b, ...c] -> List<Int>` (literal sources, static flatten)",
        wave: "R12",
        corpus: "r12_list_spread_into_return",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; literal spread sources flattened to the runtime \
                scalar-list materialiser; wasm + llvm-native legs proven in \
                relon-codegen-llvm::inplace_return_four_way::list_spread_* and \
                relon-codegen-llvm::aot_wasm_parity::r12_list_spread)",
    },
    SurfaceEntry {
        construct: "list spread `[a, ...xs, b] -> List<Int>/List<Float>` (single RUNTIME source: \
                    a List<scalar> parameter / computed handle, with optional static scalars on \
                    either side)",
        wave: "R12",
        corpus: "r12_list_spread_param_src",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; source length read from the record header, payload \
                spliced via memory.copy into a fresh scratch record, static scalars stored inline; \
                wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r12_list_spread_runtime_src)",
    },
    SurfaceEntry {
        construct: "list spread `[a, ...xs, b, ...ys, c] -> List<Int>/List<Float>` (MULTIPLE \
                    runtime sources: List<scalar> parameters / computed handles, any number, \
                    adjacent or with static Int/Float scalars interleaved, empty sources \
                    included)",
        wave: "SP",
        corpus: "r12_list_spread_multi_src_mixed",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; per-source length read from the record header, \
                total summed into one scratch record, segments written left to right with a \
                runtime write cursor folded past each copied source payload; wasm + \
                llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::r12_list_spread_multi_src and \
                relon-codegen-llvm::inplace_return_four_way::list_spread_runtime_multi_*)",
    },
    SurfaceEntry {
        construct: "dict spread `{ ...src, k: v } -> Schema` (schema-typed source, static fields)",
        wave: "R12",
        corpus: "r12_dict_spread_into_schema",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; each source field lowered as a synthesised \
                `src.field` access into the matching schema slot; wasm + llvm-native legs proven \
                in relon-codegen-llvm::inplace_return_four_way::dict_spread_* and \
                relon-codegen-llvm::aot_wasm_parity::r12_dict_spread)",
    },
    // ---- Stdlib tail wave: the last five tree-walk-only stdlib fns ----
    SurfaceEntry {
        construct: "pow(a, b) -> Float (libm pow on every leg, IEEE-754; Int operands widen)",
        wave: "ST",
        corpus: "st_pow_float",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::st_pow_* incl. int-widen / neg-exp / \
                overflow-to-inf)",
    },
    SurfaceEntry {
        construct: "count(xs) -> Int (record-header length read, any list element type)",
        wave: "ST",
        corpus: "st_count_list",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::st_count_*). The count-empty FastInt \
                shape (`count(range(0))`, no buffer operand) is not directly driven on the \
                wasm leg: object emit rejects it loudly (AllocScratch outside \
                buffer-protocol entry shape) instead of miscompiling, and the wasm leg is \
                verified through the equivalent Buffer-entry source \
                `count([1, 2].filter(..))`; a pre-existing entry-shape driving limitation, \
                not a new miscompile",
    },
    SurfaceEntry {
        construct: "every(xs, pred) -> Bool (short-circuit loop, List<Int>/List<Float>; \
                    empty list vacuously true)",
        wave: "ST",
        corpus: "st_every_true",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::st_every_* incl. the short-circuit \
                proof that stops before a trapping predicate)",
    },
    SurfaceEntry {
        construct: "some(xs, pred) -> Bool (short-circuit loop, List<Int>/List<Float>; \
                    empty list false)",
        wave: "ST",
        corpus: "st_some_true",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::st_some_* incl. the short-circuit \
                proof that stops before a trapping predicate)",
    },
    SurfaceEntry {
        construct: "unique(xs) -> Bool (O(N^2) i<j scan, List<Int>/List<Float>; \
                    OrderedFloat equality: NaN==NaN and -0.0==0.0 are duplicates)",
        wave: "ST",
        corpus: "st_unique_dup",
        status: Status::Covered,
        proof: "tree-walk + cranelift (TW_CR; wasm + llvm-native legs proven in \
                relon-codegen-llvm::aot_wasm_parity::st_unique_* incl. the NaN-dup and \
                neg-zero-dup float cases; List<String>/List<Bool> stay capped)",
    },
    // ---- stdlib symmetry wave: checked sum + min mirror ----
    SurfaceEntry {
        construct: "checked xs.sum() overflow trap (TrapKind::NumericOverflow -> \
                    NumericOverflow)",
        wave: "SYM",
        corpus: "stdlib_list_sum_overflow",
        status: Status::Covered,
        proof: "tree-walk + cranelift trap NumericOverflow on the first overflowing \
                partial sum; llvm-native leg (state-trap route) + boundary value parity \
                in relon-codegen-llvm::list_sum_overflow_four_way, wasm leg structural \
                (same emitter + trap code 6)",
    },
    SurfaceEntry {
        construct: "list min method (xs.min(), Int elements)",
        wave: "SYM",
        corpus: "stdlib_list_min",
        status: Status::Covered,
        proof: "tree-walk + cranelift + trace const; llvm-native leg + empty-trap and \
                min/max symmetry in relon-codegen-llvm::list_min_four_way (registry \
                slot 78, exact list_int_max mirror; List<Float> min stays on the \
                tree-walk fallback like Float max)",
    },
];
