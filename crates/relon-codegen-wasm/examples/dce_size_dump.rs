//! Bench helper: dump the wasm-module byte size for each DCE bench
//! scenario so the bench report can quote the absolute shrinkage.
//! Not wired into any test runner — run manually via
//! `cargo run -p relon-codegen-wasm --example dce_size_dump --release`.
//!
//! Phase v3+ b-1 added the `unused_methods` family to surface user-fn
//! DCE: an entry that ignores a schema with N dead methods should
//! collapse to the same module size as the no-schema baseline. The
//! scenarios with the `*_unused_methods*` suffix wrap an entry that
//! never touches the declared schema; DCE on must produce the same
//! byte count as the unrelated baseline.

use relon_codegen_wasm::compile_lowered_entry;
use relon_ir::lower_workspace_single;

fn compile_size(src: &str) -> usize {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    let ir = lower_workspace_single(&analyzed, &ast).expect("lower");
    compile_lowered_entry(&ir).expect("compile").len()
}

fn main() {
    let scenarios: &[(&str, &str)] = &[
        ("arithmetic", "#main(Int x, Int y) -> Int\nx * y + 1"),
        (
            "dict_literal",
            "#schema U { Int age: *, Int birth: * }\n\
             #main(Int x) -> U\n\
             { age: x, birth: 2026 - x }",
        ),
        ("stdlib_length", "#main(String s) -> Int\ns.length()"),
        (
            "list_int_map",
            "#main(List<Int> xs) -> List<Int>\nxs.map((Int x) => x * 2)",
        ),
        (
            "list_int_filter",
            "#main(List<Int> xs) -> List<Int>\nxs.filter((Int x) => x > 0)",
        ),
        (
            "list_int_fold",
            "#main(List<Int> xs) -> Int\nxs.fold(0, (Int acc, Int x) => acc + x)",
        ),
        // v3+ b-1 user-fn DCE coverage. Baseline: no schema, no
        // methods. Three variants of progressively more declared-
        // but-unused methods on a schema the entry never touches.
        ("arith_baseline", "#main(Int x) -> Int\nx * 2"),
        (
            "arith_with_3_unused_methods",
            "#schema U { Int x: * } with {\n  \
                a() -> Int: self.x\n  \
                b() -> Int: self.x * 2\n  \
                c() -> Int: self.x + 1\n\
             }\n\
             #main(Int x) -> Int\nx * 2",
        ),
        (
            "arith_with_5_unused_methods",
            "#schema U { Int x: * } with {\n  \
                a() -> Int: self.x\n  \
                b() -> Int: self.x * 2\n  \
                c() -> Int: self.x + 1\n  \
                d() -> Int: self.x - 1\n  \
                e() -> Int: self.x * self.x\n\
             }\n\
             #main(Int x) -> Int\nx * 2",
        ),
        (
            "arith_with_5_unused_methods_plus_one_used",
            "#schema U { Int x: * } with {\n  \
                a() -> Int: self.x\n  \
                b() -> Int: self.x * 2\n  \
                c() -> Int: self.x + 1\n  \
                d() -> Int: self.x - 1\n  \
                e() -> Int: self.x * self.x\n\
             }\n\
             #main(U u) -> Int\nu.a()",
        ),
    ];
    for (name, src) in scenarios {
        println!("{}: {} bytes", name, compile_size(src));
    }
}
