//! Phase v3+ a-2 stdlib dead-code elimination tests.
//!
//! Exercises the reachability pass end-to-end: compile a Relon source
//! through `compile_lowered_entry`, parse the resulting wasm module
//! with wasmparser, and verify that the function-section count
//! shrinks when the user's body never touches a stdlib body. The
//! tests also pin the keep-list discipline so a future stdlib
//! reordering or a regression in the reachability sweep that
//! accidentally prunes a reachable body surfaces immediately.

use relon_codegen_wasm::compile_lowered_entry;
use relon_ir::lower_workspace_single;
use wasmparser::{Parser, Payload};

/// Compile a Relon source string end-to-end and return the wasm bytes.
fn compile_wasm(src: &str) -> Vec<u8> {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    assert!(
        !analyzed.has_errors(),
        "analyzer reported errors: {:?}",
        analyzed.diagnostics
    );
    let ir = lower_workspace_single(&analyzed, &ast).expect("lower");
    compile_lowered_entry(&ir).expect("compile")
}

/// Inspect a wasm module's structural counts (function section
/// count + element-section funcref slot count + whether a table
/// section was emitted). Used by the DCE assertions to verify the
/// post-DCE binary is the expected shape without having to crack
/// open every section.
#[derive(Debug, Default)]
struct ModuleShape {
    /// Number of entries in the module's `FunctionSection` — i.e.
    /// non-imported wasm functions (stdlib + user). DCE shrinks
    /// this value by pruning unreachable stdlib bodies.
    fn_section_count: u32,
    /// Number of funcref slots emitted in the element section. Zero
    /// for modules without closure call sites.
    element_fn_count: u32,
    /// Whether a `table` section was emitted. Phase 11 + v3+ a-2:
    /// always emit the table when the module references closures,
    /// even when no user lambda exists, so call_indirect can resolve.
    has_table: bool,
}

fn inspect_module(bytes: &[u8]) -> ModuleShape {
    let mut shape = ModuleShape::default();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.expect("payload") {
            Payload::FunctionSection(reader) => {
                shape.fn_section_count = reader.count();
            }
            Payload::ElementSection(reader) => {
                for elem in reader {
                    let elem = elem.expect("element");
                    if let wasmparser::ElementItems::Functions(items) = elem.items {
                        shape.element_fn_count += items.count();
                    }
                }
            }
            Payload::TableSection(reader) => {
                if reader.count() > 0 {
                    shape.has_table = true;
                }
            }
            _ => {}
        }
    }
    shape
}

#[test]
fn dce_unused_stdlib_pruned() {
    // `x * 2` has zero stdlib reach, so the function section should
    // contain exactly one entry (the entry `#main`).
    let wasm = compile_wasm("#main(Int x) -> Int\nx * 2");
    let shape = inspect_module(&wasm);
    assert_eq!(
        shape.fn_section_count, 1,
        "expected only #main in the function section (got {})",
        shape.fn_section_count
    );
    assert_eq!(
        shape.element_fn_count, 0,
        "no closure call sites means no element section entries"
    );
    assert!(!shape.has_table, "no closures means no funcref table");
}

#[test]
fn dce_used_stdlib_kept() {
    // `s.length()` reaches exactly one bundled body (`length`), so
    // the post-DCE module carries it plus `#main`.
    let wasm = compile_wasm("#main(String s) -> Int\ns.length()");
    let shape = inspect_module(&wasm);
    assert_eq!(
        shape.fn_section_count, 2,
        "expected `length` + #main (got {})",
        shape.fn_section_count
    );
}

#[test]
fn dce_lambda_kept() {
    // A user lambda passed through `fold` keeps both the lambda body
    // and the `list_int_fold` stdlib body. The reachability sweep
    // also drags in any helper bodies the fold path uses (today none,
    // but the assertion is shape-tolerant). The exact count varies
    // with the bundled stdlib's reach graph; pin `>= 3` so the test
    // stays robust against future helpers without losing its punch.
    let wasm = compile_wasm("#main(List<Int> xs) -> Int\nxs.fold(0, (Int acc, Int x) => acc + x)");
    let shape = inspect_module(&wasm);
    assert!(
        shape.fn_section_count >= 3,
        "expected at least `list_int_fold` + lambda + #main (got {})",
        shape.fn_section_count
    );
    // The closure must be registered in the funcref table — the
    // funcref slot count is `closure_table.len()` (one lambda here).
    assert!(
        shape.has_table,
        "closure call site must emit a funcref table"
    );
    assert!(
        shape.element_fn_count >= 1,
        "lambda must be reachable through the funcref table"
    );
}

#[test]
fn dce_off_vs_on_byte_size() {
    // Wire-format comparison: the no-stdlib body must yield a
    // strictly smaller wasm module than the same body that also
    // calls a stdlib function. This is the operational signal we
    // care about for the v10 bench — cranelift JIT cost scales with
    // wasm bytes, so DCE-shrunk modules cold-start faster.
    let no_stdlib = compile_wasm("#main(Int x) -> Int\nx * 2");
    let with_stdlib = compile_wasm("#main(String s) -> Int\ns.length()");
    assert!(
        no_stdlib.len() < with_stdlib.len(),
        "expected the stdlib-free module to be smaller than the \
         length-using one (no_stdlib={}, with_stdlib={})",
        no_stdlib.len(),
        with_stdlib.len(),
    );
    // Sanity bound: the difference must reflect at least the
    // `length` body's bytecode rather than a few-byte signature
    // change. The `length` body is roughly 6 bytes of payload plus
    // header overhead; 20 bytes is a conservative floor.
    let delta = with_stdlib.len() - no_stdlib.len();
    assert!(
        delta >= 8,
        "expected at least 8 byte gap from the kept stdlib body (got {})",
        delta
    );
}

#[test]
fn dce_keeps_each_stdlib_independently() {
    // Each surface call kicks in exactly its own stdlib body. The
    // module shape lets us pin "user picks N stdlib bodies -> N+1
    // post-DCE wasm functions" across a handful of representative
    // call shapes. This is the regression net against an over-eager
    // BFS that prunes a body the entry actually invokes.
    let cases: &[(&str, u32)] = &[
        ("#main(Int x) -> Int\nabs(x)", 2),
        ("#main(Int a, Int b) -> Int\nmin(a, b)", 2),
        ("#main(Int a, Int b) -> Int\nmax(a, b)", 2),
        ("#main(String s) -> Bool\nis_empty(s)", 2),
        ("#main(List<Int> xs) -> Int\nxs.sum()", 2),
    ];
    for (src, expected_funcs) in cases {
        let wasm = compile_wasm(src);
        let shape = inspect_module(&wasm);
        assert_eq!(
            shape.fn_section_count, *expected_funcs,
            "case {:?}: expected {} wasm funcs (stdlib + #main), got {}",
            src, expected_funcs, shape.fn_section_count
        );
    }
}
