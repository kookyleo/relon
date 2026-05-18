fn main() {
    let src = "#main(Int x) -> Int\nabs(x)";
    let ast = relon_parser::parse_document(src).unwrap();
    let analyzed = relon_analyzer::analyze(&ast);
    for d in &analyzed.diagnostics {
        eprintln!("DIAG: {:?}", d);
    }
    let lowered = relon_ir::lower_workspace_single(&analyzed, &ast).unwrap();
    let entry_idx = lowered.module.entry_func_index.unwrap();
    let func = &lowered.module.funcs[entry_idx];
    println!("params: {:?}", func.params);
    for (i, op) in func.body.iter().enumerate() {
        println!("  {}: {:?}", i, op.op);
    }
}
