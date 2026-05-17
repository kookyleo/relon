//! Bench helper: dump the wasm-module byte size for each Phase v3+
//! a-2 bench scenario so the bench report can quote the absolute
//! shrinkage. Not wired into any test runner — run manually via
//! `cargo run -p relon-codegen-wasm --example dce_size_dump --release`.

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
    ];
    for (name, src) in scenarios {
        println!("{}: {} bytes", name, compile_size(src));
    }
}
