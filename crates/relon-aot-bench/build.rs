//! Drives the LLVM-AOT compile of every bench `.relon` source into a
//! native `.o` linked into this binary. Workload `we` calls a host
//! `#native` fn `mix` — closed-world co-compiled (inlined into the .o).

fn main() {
    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo");

    // Host fn for workload (e). No capability gate (ungated) so the
    // binding is a plain `i64` (no Result), matching the hand-written
    // Rust direct-call baseline exactly.
    let mix = relon_rs_build::NativeHostFn {
        name: "mix".to_string(),
        param_types: vec!["Int".to_string()],
        return_type: "Int".to_string(),
        gate: relon_rs_build::NativeFnGate::default(),
        rust_impl: r#"
#[no_mangle]
pub extern "C" fn mix(x: i64) -> i64 {
    // Cheap integer hash; the point is a per-iteration host-fn call
    // site, closed-world inlined into the .o.
    x.wrapping_mul(2654435761).wrapping_add(x >> 3)
}
"#
        .to_string(),
    };

    relon_rs_build::Compiler::new()
        .source("src/wa.relon")
        .source("src/wb.relon")
        .source("src/wc.relon")
        .source("src/wd.relon")
        .source("src/wf.relon")
        .source("src/wg.relon")
        .source_with_native_fns("src/we.relon", vec![mix])
        .opt_level(3)
        .emit_all(&out_dir)
        .expect("relon-rs-build: compile bench sources");

    println!("cargo:rerun-if-changed=build.rs");
}
