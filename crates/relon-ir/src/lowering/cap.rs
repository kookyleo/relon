//! Capability-cap site registry for the lowering pass.
//!
//! Every place in `lowering` that constructs a [`crate::error::LoweringError`] — a loud
//! "this construct is not yet expressible in the IR" cap — is wrapped in
//! the `cap!` macro with a stable, descriptive id. The wrapping is
//! deliberately codegen-neutral:
//!
//! * In a normal (non-test) build `cap!(id, err)` is a **pure identity
//!   passthrough**: it expands to `err` unchanged, the id is consumed
//!   only as a compile-time `&'static str` constant, and no runtime code
//!   is emitted. The IR / object bytes the backends hash therefore stay
//!   byte-identical to a tree without the wrapping, so the codegen
//!   `GENERATOR_VERSION` does not move.
//! * Under `#[cfg(test)]` `cap!` additionally records that `id` fired
//!   (see `record`) before returning `err` unchanged. A later wave's
//!   no-fallback test reads `fired_caps` to learn which caps a corpus
//!   probe actually hit. The recording is side-effect-free with respect
//!   to the returned error value.
//!
//! [`LOWERING_CAP_IDS`] is the canonical list of every wrapped site's id.
//! The macro asserts (in test builds) that it is only ever invoked with an
//! id present in this slice, and the harness `ledger_completeness` test
//! enforces a bijection between this slice and the test-harness ledger, so
//! a future cap added without a registered id fails fast.

/// Wrap a [`crate::error::LoweringError`] construction at a capped site.
///
/// `cap!(id, err_expr)` returns `err_expr` unchanged. `id` must be a
/// `&'static str` literal that appears in [`LOWERING_CAP_IDS`].
///
/// The two arms below are value-identical; only the test arm adds the
/// (value-preserving) recording side-channel.
#[cfg(not(test))]
macro_rules! cap {
    ($id:expr, $err:expr $(,)?) => {{
        // Consume the id as a compile-time constant so a typo is still a
        // type error, without emitting any runtime instruction.
        const _: &str = $id;
        $err
    }};
}

#[cfg(test)]
macro_rules! cap {
    ($id:expr, $err:expr $(,)?) => {{
        const _: &str = $id;
        $crate::lowering::cap::record($id);
        $err
    }};
}

/// Stable ids for every `cap!` site in the lowering pass. Append-only in
/// spirit: a new cap site must add its id here (and a ledger entry), or the
/// completeness test fails. Ids are `<fn>.<variant_snake>[.<n>]`, where the
/// trailing index disambiguates multiple physical sites of the same variant
/// inside one function.
pub const LOWERING_CAP_IDS: &[&str] = &[
    "lower_workspace.entry_module_not_found.1",
    "lower_workspace.entry_module_not_found.2",
    "lower_workspace.multiple_main_directives",
    "detect_cross_file_schema_conflicts.duplicate_schema_across_files",
    "lower_entry_with_resolver.missing_main",
    "lower_entry_with_resolver.closure_across_boundary.1",
    "lower_entry_with_resolver.closure_across_boundary.2",
    "lower_entry_with_resolver.unsupported_expr.1",
    "lower_entry_with_resolver.unsupported_expr.2",
    "lower_entry_with_resolver.unsupported_type_in_main",
    "lower_entry_with_resolver.unsupported_expr.3",
    "build_main_params_schema.unsupported_type_in_main",
    "build_main_return_schema.unsupported_type_in_main.1",
    "build_main_return_schema.unsupported_type_in_main.2",
    "return_tuple_canonical.unsupported_type_in_main",
    "lower_tuple_return.unsupported_expr",
    "lower_tuple_return.arity_mismatch",
    "desugar_field_decorators.unsupported_expr.1",
    "desugar_field_decorators.unsupported_expr.2",
    "desugar_field_decorators.unsupported_expr.3",
    "anon_dict_return_plan.unsupported_expr",
    "anon_dict_return_plan.closure_across_boundary",
    "anon_dict_return_plan.unsupported_field_type",
    "anon_dict_emit_order.cyclic_field_dependency",
    "anon_dict_emit_order.forward_ref_through_param",
    "classify_anon_dict_scalar_field.unsupported_expr",
    "classify_anon_dict_str_int_field.unsupported_expr.1",
    "classify_anon_dict_str_int_field.unsupported_expr.2",
    "classify_anon_dict_list_string_field.unsupported_expr",
    "classify_anon_dict_list_field.unsupported_field_type.1",
    "classify_anon_dict_list_field.unsupported_field_type.2",
    "classify_anon_dict_list_field.unsupported_field_type.3",
    "classify_anon_dict_scalar_field_irt.unsupported_expr.1",
    "classify_anon_dict_scalar_field_irt.unsupported_expr.2",
    "classify_anon_dict_scalar_field_irt.unsupported_expr.3",
    "classify_anon_dict_scalar_field_irt.unsupported_expr.4",
    "classify_anon_dict_scalar_field_irt.reference_unresolved",
    "lower_anon_dict_body.unsupported_expr.1",
    "lower_anon_dict_body.unsupported_expr.2",
    "lower_anon_dict_body.unsupported_expr.3",
    "lower_anon_dict_body.unsupported_field_type.1",
    "lower_anon_dict_body.unsupported_expr.4",
    "lower_anon_dict_body.unsupported_field_type.2",
    "lower_anon_dict_body.unsupported_expr.5",
    "lower_anon_dict_body.unsupported_expr.6",
    "lower_anon_dict_body.unsupported_expr.7",
    "lower_anon_dict_body.unsupported_field_type.3",
    "lower_anon_dict_body.unsupported_expr.8",
    "type_repr_to_ir_type.unsupported_type_in_main.1",
    "type_repr_to_ir_type.unsupported_type_in_main.2",
    "type_repr_to_ir_type_dict.unsupported_type_in_main",
    "lower_expr.unsupported_expr.1",
    "lower_expr.unsupported_expr.2",
    "lower_expr.unsupported_expr.3",
    "lower_expr.unsupported_expr.4",
    "lower_expr.unsupported_expr.5",
    "lower_expr.unsupported_expr.6",
    "lower_expr.unsupported_expr.7",
    "lower_expr.unsupported_expr.spread_empty",
    "lower_expr.unsupported_expr.spread_elem_ty",
    "lower_expr.closure_across_boundary",
    "lower_expr.unsupported_expr.8",
    "try_lower_local_closure_call.unsupported_expr.1",
    "try_lower_local_closure_call.unknown_stdlib_method",
    "try_lower_local_closure_call.unsupported_expr.2",
    "try_lower_local_closure_call.unsupported_expr.3",
    "try_lower_local_closure_call.stdlib_arg_type_mismatch",
    "try_lower_native_call.unsupported_expr.1",
    "try_lower_native_call.unsupported_expr.2",
    "try_lower_native_call.unsupported_expr.3",
    "lower_fn_call.unsupported_expr.1",
    "lower_fn_call.unsupported_expr.2",
    "lower_fn_call.unknown_stdlib_method.1",
    "lower_fn_call.unknown_stdlib_method.2",
    "lower_fn_call.unknown_stdlib_method.3",
    "lower_fn_call.unsupported_expr.3",
    "lower_fn_call.unsupported_expr.4",
    "lower_fn_call.unknown_stdlib_method.4",
    "lower_fn_call.unknown_stdlib_method.5",
    "lower_fn_call.unknown_stdlib_method.6",
    "lower_fn_call.split_empty_separator",
    "lower_fn_call.unsupported_expr.5",
    "lower_fn_call.unsupported_expr.6",
    "lower_list_index_typed.unsupported_expr",
    "lower_list_string_index.unsupported_expr",
    "lower_dict_string_index.unsupported_expr.1",
    "lower_dict_string_index.unsupported_expr.2",
    "lower_dict_string_index.unsupported_expr.3",
    "expect_int_top.unsupported_expr.1",
    "expect_int_top.unsupported_expr.2",
    "lower_stdlib_arg.unknown_stdlib_method",
    "lower_stdlib_arg.unsupported_expr.1",
    "lower_stdlib_arg.unsupported_expr.2",
    "lower_stdlib_arg.unsupported_expr.3",
    "lower_method_receiver.unsupported_expr.1",
    "lower_method_receiver.unsupported_expr.2",
    "finish_schema_method_call.unknown_stdlib_method",
    "finish_schema_method_call.unsupported_expr.1",
    "finish_schema_method_call.stdlib_arg_type_mismatch.1",
    "finish_schema_method_call.unsupported_expr.2",
    "finish_schema_method_call.unsupported_expr.3",
    "finish_schema_method_call.stdlib_arg_type_mismatch.2",
    "check_stdlib_arg.unknown_stdlib_method",
    "check_stdlib_arg.stdlib_arg_type_mismatch",
    "lower_where.unsupported_expr.1",
    "lower_where.unsupported_expr.2",
    "lower_where.unsupported_expr.3",
    "lower_where.unsupported_expr.4",
    "lower_where.unsupported_expr.5",
    "lower_where.unsupported_expr.6",
    "lower_binary.unsupported_operator.1",
    "lower_binary.unsupported_operator.2",
    "lower_binary.unsupported_operator.3",
    "lower_binary.unsupported_operator.4",
    "lower_binary.unsupported_operator.5",
    "lower_binary.unsupported_operator.6",
    "lower_binary.unsupported_operator.7",
    "lower_binary.unsupported_operator.8",
    "lower_binary.unsupported_operator.9",
    "lower_binary.unsupported_operator.10",
    "lower_binary.unsupported_operator.11",
    "lower_binary.unsupported_operator.12",
    "lower_binary.unsupported_operator.13",
    "lower_binary.unsupported_operator.14",
    "lower_ternary.unsupported_expr",
    "lower_ternary.if_condition_not_bool",
    "lower_ternary.if_branch_type_mismatch",
    "lower_ternary_as_type.unsupported_expr",
    "lower_ternary_as_type.if_condition_not_bool",
    "lower_ternary_as_type.if_branch_type_mismatch",
    "lower_ternary_as_type.unsupported_expr.branch_type",
    "lower_branch.unsupported_expr",
    "lower_branch_as_type.unsupported_expr",
    "canonical_schema_from_def.unsupported_expr",
    "canonical_schema_from_def.cyclic_field_dependency",
    "canonical_schema_from_def.unsupported_field_type",
    "canonical_type_repr.unsupported_field_type.1",
    "canonical_type_repr.unsupported_field_type.2",
    "canonical_type_repr.unsupported_field_type.3",
    "topo_order_fields.missing_field_no_default",
    "topo_order_fields.cyclic_field_dependency",
    "check_field_default_refs_resolvable.unknown_field_reference_in_default",
    "lower_dict_into_record.unknown_schema_brand",
    "spread_source_schema.non_variable",
    "spread_source_schema.non_string_head",
    "spread_source_schema.non_string_segment",
    "spread_source_schema.not_a_schema",
    "lower_dict_into_record.duplicate_spread_field",
    "lower_dict_into_record.duplicate_field",
    "lower_dict_into_record.unsupported_expr.1",
    "lower_dict_into_record.unsupported_field_type.1",
    "lower_dict_into_record.unsupported_expr.2",
    "lower_dict_into_record.unsupported_field_type.2",
    "lower_dict_into_record.unsupported_field_type.3",
    "lower_dict_field_value.unsupported_expr.1",
    "lower_dict_field_value.unsupported_field_type.1",
    "lower_dict_field_value.unsupported_field_type.2",
    "lower_dict_field_value.unsupported_expr.2",
    "lower_dict_field_value.unsupported_field_type.3",
    "lower_dict_default.missing_field_no_default",
    "lower_reference.positional_base",
    "lower_reference.unsupported_path_shape",
    "lower_reference.unresolved_field",
    "lower_variable.unsupported_expr.1",
    "lower_variable.unsupported_expr.2",
    "lower_variable.unresolved_variable.1",
    "lower_variable.unresolved_variable.2",
    "lower_variable.unsupported_expr.3",
    "lower_variable.unsupported_expr.4",
    "lower_variable.unsupported_expr.5",
    "lower_variable.unsupported_expr.6",
    "lower_variable.unsupported_expr.7",
    "lower_variable.unsupported_expr.8",
    "lower_variable.unsupported_field_type",
    "lower_variable.unsupported_expr.9",
    "lower_variable.unsupported_expr.10",
    "method_signature_ir_types.unsupported_type_in_main.1",
    "method_signature_ir_types.unsupported_type_in_main.2",
    "method_signature_ir_types.unsupported_type_in_main.3",
    "lower_one_method.unsupported_expr.1",
    "lower_one_method.unsupported_expr.2",
    "lower_one_method.unsupported_type_in_main",
    "try_lower_materialized_list_reduce.unresolved_variable",
    "try_lower_materialized_list_reduce.unsupported_expr.1",
    "try_lower_materialized_list_reduce.unsupported_expr.2",
    "try_lower_materialized_list_reduce.unsupported_expr.3",
    "combine_operator_to_op.unsupported_expr",
    "try_lower_scalar_math.unknown_stdlib_method.1",
    "try_lower_scalar_math.unknown_stdlib_method.2",
    "try_lower_predicate_math.unknown_stdlib_method.1",
    "try_lower_predicate_math.unknown_stdlib_method.2",
    "try_lower_size_in_range.string_charcount_capped",
    "try_lower_size_in_range.unknown_stdlib_method.1",
    "try_lower_size_in_range.unknown_stdlib_method.2",
    "try_lower_list_filter.unknown_stdlib_method.1",
    "try_lower_list_filter.unknown_stdlib_method.2",
    "try_lower_list_filter.unsupported_expr",
    "try_lower_list_unique.unknown_stdlib_method.1",
    "try_lower_list_unique.unknown_stdlib_method.2",
    "emit_list_int_hof_call.unknown_stdlib_method.1",
    "emit_list_int_hof_call.unknown_stdlib_method.2",
    "emit_list_int_hof_call.unsupported_expr",
    "emit_list_int_fold_call.unknown_stdlib_method.1",
    "emit_list_int_fold_call.unknown_stdlib_method.2",
    "emit_list_int_fold_call.unsupported_expr.1",
    "emit_list_int_fold_call.unsupported_expr.2",
    "lower_comprehension.unsupported_expr.1",
    "lower_comprehension.unsupported_expr.2",
    "lower_comprehension.unsupported_expr.3",
    "lower_comprehension.unknown_stdlib_method.1",
    "lower_comprehension.unknown_stdlib_method.2",
    "try_lower_list_sum_value.unknown_stdlib_method.1",
    "try_lower_list_sum_value.unknown_stdlib_method.2",
    "emit_list_float_literal_materialize.unsupported_expr.1",
    "emit_list_float_literal_materialize.unsupported_expr.2",
    "emit_list_float_literal_materialize.unsupported_expr.3",
    "emit_list_float_literal_materialize.unsupported_expr.4",
    "emit_list_int_literal_materialize.unsupported_expr.1",
    "emit_list_int_literal_materialize.unsupported_expr.2",
    "emit_list_int_literal_materialize.unsupported_expr.3",
    "emit_list_int_literal_materialize.unsupported_expr.4",
    "probe_expr_ir_ty.unsupported_expr",
    "flatten_list_spread.unsupported_spread_source",
    "emit_list_spread_runtime_materialize.unsupported_expr.1",
    "emit_list_spread_runtime_materialize.unsupported_source_ty",
    "emit_list_spread_runtime_materialize.mixed_source_ty",
    "emit_list_spread_runtime_materialize.unsupported_expr.static_count_overflow",
    "emit_scalar_element_value_store.unsupported_expr.1",
    "emit_scalar_element_value_store.unsupported_expr.2",
    "emit_scalar_element_value_store.unsupported_expr.3",
    "emit_list_value_materialize.unsupported_expr.1",
    "emit_list_value_materialize.unsupported_expr.2",
    "emit_list_value_materialize.unsupported_expr.3",
    "emit_list_value_materialize.unsupported_expr.4",
    "emit_range_pipeline_loop.unsupported_expr.1",
    "emit_range_pipeline_loop.unsupported_expr.2",
    "emit_range_pipeline_loop.unsupported_expr.3",
    "emit_range_pipeline_loop.unsupported_expr.4",
    "emit_range_pipeline_loop.unsupported_expr.5",
    "emit_range_pipeline_loop.unsupported_expr.6",
    "emit_range_pipeline_loop.unsupported_expr.7",
    "emit_range_pipeline_loop.unsupported_expr.8",
    "resolve_capture.unsupported_closure_capture",
    "lower_closure_as_value.unsupported_expr.1",
    "lower_closure_as_value.unsupported_expr.2",
    "lower_closure_as_value.unsupported_expr.3",
    "lower_closure_as_value.stdlib_arg_type_mismatch",
    "lower_match.unsupported_expr.1",
    "lower_match.unsupported_expr.3",
    "lower_match.empty_enum_match",
    "lower_match.no_match_trap_result_ty",
    "canonical_schema_from_def.unsupported_tuple_element_type",
    "canonical_type_repr.unsupported_field_type.generics",
    "canonical_type_repr.unsupported_field_type.reserved",
    "canonical_type_repr.unsupported_field_type.tuple",
    "classify_anon_dict_enum_list_field.unsupported_field_type.1",
    "classify_anon_dict_enum_list_field.unsupported_field_type.2",
    "classify_anon_dict_enum_list_field.unsupported_field_type.3",
    "classify_anon_dict_enum_list_field.unsupported_field_type.4",
    "classify_anon_dict_variant_list_field.unsupported_field_type.1",
    "classify_anon_dict_variant_list_field.unsupported_field_type.2",
    "classify_anon_dict_variant_list_field.unsupported_field_type.3",
    "classify_anon_dict_variant_list_field.unsupported_field_type.4",
    "classify_anon_dict_variant_list_field.unsupported_field_type.5",
    "emit_standard_variant_record.empty_payload_stack",
    "emit_standard_variant_record.missing_payload",
    "emit_standard_variant_record.payload_type_mismatch",
    "emit_standard_variant_record.unexpected_payload",
    "lower_dict_field_value.unsupported_expr.variant_list_stack_empty",
    "lower_dict_field_value.unsupported_field_type.variant_list_stack",
    "lower_match.enum_pattern_binding_stack",
    "lower_match.enum_pattern_duplicate_binding",
    "lower_match.enum_pattern_unit_payload",
    "lower_match.enum_pattern_unknown_payload",
    "lower_plain_dict_into_record.missing_field",
    "lower_plain_dict_into_record.unsupported_expr.1",
    "lower_plain_dict_into_record.unsupported_expr.2",
    "lower_plain_dict_into_record.unsupported_field_type.1",
    "lower_plain_dict_into_record.unsupported_field_type.2",
    "lower_plain_dict_into_record.unsupported_field_type.3",
    "lower_plain_dict_into_record.unsupported_field_type.4",
    "lower_prelude_variant_call_as_type.arity_mismatch",
    "lower_schema_value_as_absolute_pointer.arity_mismatch",
    "lower_tuple_field.arity_mismatch",
    "lower_variable.unsupported_expr.enum_payload_missing_layout",
    "lower_variable.unsupported_expr.enum_payload_path",
    "lower_variable.unsupported_expr.enum_payload_segment",
    "lower_variable.unsupported_expr.enum_payload_stack",
    "lower_variable.unsupported_expr.enum_payload_stack_type",
    "lower_variable.unsupported_expr.enum_payload_unknown_field",
    "lower_variable.unsupported_expr.enum_unit_payload_access",
    "lower_variant_ctor_as_type.body_not_dict",
    "lower_variant_ctor_as_type.duplicate_payload",
    "lower_variant_ctor_as_type.missing_payload",
    "lower_variant_ctor_as_type.non_string_key",
    "lower_variant_ctor_as_type.unexpected_field",
    "lower_variant_ctor_as_type.unit_variant_has_fields",
    "lower_variant_record_list_literal.element_type_mismatch",
    "lower_variant_record_list_literal.empty_element_stack",
    "lower_variant_record_list_literal.length_overflow",
    "payload_slot_layout_for_lowering.unsupported_type",
    "standard_variant_shape.not_variant_type",
    "standard_variant_shape.option_enum_mismatch",
    "standard_variant_shape.option_variant_mismatch",
    "standard_variant_shape.result_enum_mismatch",
    "standard_variant_shape.result_variant_mismatch",
    "type_graph_alignment_for_lowering.unsupported_type",
    "variant_record_alignment_for_lowering.unsupported_type",
];

#[cfg(test)]
thread_local! {
    static FIRED: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Record that the cap `id` fired. Test-only; asserts `id` is registered.
#[cfg(test)]
pub(crate) fn record(id: &'static str) {
    debug_assert!(
        LOWERING_CAP_IDS.contains(&id),
        "cap! fired with unregistered id `{id}` — add it to LOWERING_CAP_IDS"
    );
    FIRED.with(|f| f.borrow_mut().push(id));
}

/// Drain and return the ids that fired on this thread since the last call.
/// Test-only; used by no-fallback coverage tests to assert which cap a
/// probe hit.
#[cfg(test)]
pub(crate) fn fired_caps() -> Vec<&'static str> {
    FIRED.with(|f| std::mem::take(&mut *f.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_cap_ids<'a>(src: &'a str, out: &mut std::collections::HashSet<&'a str>) {
        let mut rest = src;
        while let Some(start) = rest.find("cap!") {
            rest = &rest[start + "cap!".len()..];
            let trimmed = rest.trim_start();
            if !trimmed.starts_with('(') {
                continue;
            }
            let after_paren = trimmed[1..].trim_start();
            if !after_paren.starts_with('"') {
                continue;
            }
            let after_quote = &after_paren[1..];
            if let Some(end) = after_quote.find('"') {
                out.insert(&after_quote[..end]);
                rest = &after_quote[end + 1..];
            } else {
                break;
            }
        }
    }

    #[test]
    fn all_cap_macro_ids_are_registered() {
        let mut used = std::collections::HashSet::new();
        collect_cap_ids(include_str!("mod.rs"), &mut used);
        collect_cap_ids(include_str!("peephole.rs"), &mut used);
        collect_cap_ids(include_str!("closure.rs"), &mut used);
        for id in used {
            assert!(
                LOWERING_CAP_IDS.contains(&id),
                "cap! id `{id}` is used but missing from LOWERING_CAP_IDS"
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for id in LOWERING_CAP_IDS {
            assert!(
                seen.insert(*id),
                "duplicate cap id in LOWERING_CAP_IDS: {id}"
            );
        }
    }

    #[test]
    fn record_then_drain_roundtrips() {
        let id = LOWERING_CAP_IDS[0];
        let _ = fired_caps(); // clear any residue from earlier tests on this thread
        record(id);
        assert_eq!(fired_caps(), vec![id]);
        assert!(fired_caps().is_empty(), "drain should empty the buffer");
    }
}
