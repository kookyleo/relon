//! Host-imports ABI table — the FROZEN contract from
//! `docs/internal/phase-z-design.md` §4.
//!
//! Each [`HostImport`] declares the module name, fn name, and the
//! `ValType` shape. The emitter consults this table when assembling
//! the WASM import section; `relon-wasm-evaluator` consults it when
//! wiring the wasmtime `Linker`.
//!
//! Adding a new import: append a new entry. The numeric `id` is the
//! freeze contract — it MUST NOT change once an entry exists, because
//! emitted modules reference imports by *type-index*, which is the
//! position in the import section.
//!
//! Removing an entry breaks every previously-emitted module. Don't.

use wasm_encoder::ValType;

/// One host-imports table entry.
///
/// The `id` field is the design-doc table number (§4.1 - §4.6). It is
/// surfaced to test code so a future migration that re-orders the
/// table catches the drift via a stamp test rather than silent
/// emit-time bugs.
#[derive(Debug, Clone, Copy)]
pub struct HostImport {
    /// Frozen numeric id per design doc §4.1 - §4.6.
    pub id: u32,
    /// Always "relon" for Phase Z.
    pub module: &'static str,
    /// Function name as the WASM import.
    pub name: &'static str,
    /// Parameter types in declaration order.
    pub params: &'static [ValType],
    /// Result types (Phase Z uses at most one result per fn).
    pub results: &'static [ValType],
}

/// FROZEN host-imports table.
///
/// Order matches design doc §4.1 - §4.6 verbatim. Each entry's index
/// in this slice is also the function-import index emitted modules
/// reference.
///
/// Z.1 only wires the subset the POC workloads exercise. The remaining
/// entries are declared so the table is complete for the freeze;
/// `relon-wasm-evaluator` registers stubs for the unimplemented ones
/// that trap `unreachable` if called (which they can't be from a Z.1
/// program, since the emitter never references them).
pub const HOST_IMPORTS: &[HostImport] = &[
    // === §4.1 Arena / control ===
    HostImport {
        id: 1,
        module: "relon",
        name: "__relon_arena_alloc",
        params: &[ValType::I32, ValType::I32], // size, align
        results: &[ValType::I32],              // ptr
    },
    HostImport {
        id: 2,
        module: "relon",
        name: "__relon_arena_reset",
        params: &[],
        results: &[],
    },
    HostImport {
        id: 3,
        module: "relon",
        name: "__relon_arena_grow",
        params: &[ValType::I32],  // pages
        results: &[ValType::I32], // prev_pages
    },
    HostImport {
        id: 4,
        module: "relon",
        name: "__relon_trap",
        params: &[ValType::I32], // trap code
        results: &[],            // never returns; wasmtime trap raises
    },
    // === §4.2 String ===
    HostImport {
        id: 5,
        module: "relon",
        name: "__relon_str_intern",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 6,
        module: "relon",
        name: "__relon_str_alloc",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 7,
        module: "relon",
        name: "__relon_str_len",
        params: &[ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 8,
        module: "relon",
        name: "__relon_str_byte_len",
        params: &[ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 9,
        module: "relon",
        name: "__relon_str_eq",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 10,
        module: "relon",
        name: "__relon_str_concat",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 11,
        module: "relon",
        name: "__relon_str_concat_n",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 12,
        module: "relon",
        name: "__relon_str_contains",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 13,
        module: "relon",
        name: "__relon_str_substring",
        params: &[ValType::I32, ValType::I64, ValType::I64],
        results: &[ValType::I32],
    },
    HostImport {
        id: 14,
        module: "relon",
        name: "__relon_str_glob_match",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    // === §4.3 List ===
    HostImport {
        id: 15,
        module: "relon",
        name: "__relon_list_new",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 16,
        module: "relon",
        name: "__relon_list_len",
        params: &[ValType::I32],
        results: &[ValType::I64],
    },
    HostImport {
        id: 17,
        module: "relon",
        name: "__relon_list_push_i64",
        params: &[ValType::I32, ValType::I64],
        results: &[],
    },
    HostImport {
        id: 18,
        module: "relon",
        name: "__relon_list_push_f64",
        params: &[ValType::I32, ValType::F64],
        results: &[],
    },
    HostImport {
        id: 19,
        module: "relon",
        name: "__relon_list_get_i64",
        params: &[ValType::I32, ValType::I64],
        results: &[ValType::I64],
    },
    HostImport {
        id: 20,
        module: "relon",
        name: "__relon_list_get_f64",
        params: &[ValType::I32, ValType::I64],
        results: &[ValType::F64],
    },
    HostImport {
        id: 21,
        module: "relon",
        name: "__relon_list_set_i64",
        params: &[ValType::I32, ValType::I64, ValType::I64],
        results: &[],
    },
    HostImport {
        id: 22,
        module: "relon",
        name: "__relon_list_set_f64",
        params: &[ValType::I32, ValType::I64, ValType::F64],
        results: &[],
    },
    HostImport {
        id: 23,
        module: "relon",
        name: "__relon_list_range_alloc",
        params: &[ValType::I64, ValType::I64],
        results: &[ValType::I32],
    },
    HostImport {
        id: 24,
        module: "relon",
        name: "__relon_list_sum_i64",
        params: &[ValType::I32],
        results: &[ValType::I64],
    },
    HostImport {
        id: 25,
        module: "relon",
        name: "__relon_list_sum_f64",
        params: &[ValType::I32],
        results: &[ValType::F64],
    },
    // === §4.4 Dict ===
    HostImport {
        id: 26,
        module: "relon",
        name: "__relon_dict_new",
        params: &[ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 27,
        module: "relon",
        name: "__relon_dict_get_str",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I64],
    },
    HostImport {
        id: 28,
        module: "relon",
        name: "__relon_dict_set_str",
        params: &[ValType::I32, ValType::I32, ValType::I32, ValType::I64],
        results: &[],
    },
    HostImport {
        id: 29,
        module: "relon",
        name: "__relon_dict_contains_str",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 30,
        module: "relon",
        name: "__relon_dict_len",
        params: &[ValType::I32],
        results: &[ValType::I64],
    },
    // === §4.5 Closure ===
    HostImport {
        id: 31,
        module: "relon",
        name: "__relon_closure_alloc",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I32],
    },
    HostImport {
        id: 32,
        module: "relon",
        name: "__relon_closure_capture_set",
        params: &[ValType::I32, ValType::I32, ValType::I64],
        results: &[],
    },
    HostImport {
        id: 33,
        module: "relon",
        name: "__relon_closure_capture_get",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I64],
    },
    HostImport {
        id: 34,
        module: "relon",
        name: "__relon_closure_fn_idx",
        params: &[ValType::I32],
        results: &[ValType::I32],
    },
    // === §4.6 Native / capability ===
    HostImport {
        id: 35,
        module: "relon",
        name: "__relon_check_cap",
        params: &[ValType::I32],
        results: &[],
    },
    HostImport {
        id: 36,
        module: "relon",
        name: "__relon_call_native",
        params: &[ValType::I32, ValType::I32],
        results: &[ValType::I64],
    },
];

/// Look up an import by its frozen id. Used by the emitter to resolve
/// a `Call(import_idx)` operand; the index is the position in
/// [`HOST_IMPORTS`], not the design-doc number.
pub(crate) fn import_index(id: u32) -> u32 {
    HOST_IMPORTS
        .iter()
        .position(|h| h.id == id)
        .map(|i| i as u32)
        .unwrap_or_else(|| panic!("HOST_IMPORTS missing id={id} — table desync"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stamp: every host-imports entry's id matches its position + 1.
    /// If a future commit re-orders entries (or adds one mid-table),
    /// this catches the drift before any module emits the wrong index.
    #[test]
    fn host_imports_ids_are_dense_and_one_indexed() {
        for (idx, imp) in HOST_IMPORTS.iter().enumerate() {
            assert_eq!(
                imp.id as usize,
                idx + 1,
                "HOST_IMPORTS[{idx}].id = {} (expected {}); table desync",
                imp.id,
                idx + 1
            );
        }
    }

    /// Stamp: the total count tracks the design-doc total (35 declared
    /// in §4.1 - §4.6, plus the place-holder `__relon_call_native`
    /// brings it to 36). Use a debug_assert-friendly hard match so a
    /// future commit that adds an entry surfaces in the diff.
    #[test]
    fn host_imports_count_matches_design_freeze() {
        assert_eq!(HOST_IMPORTS.len(), 36);
    }
}
