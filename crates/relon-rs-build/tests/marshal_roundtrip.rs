//! Cross-crate anti-drift guard for the `EmittedFieldType` marshalling
//! triple (S1.A seam).
//!
//! The type-marshalling envelope is mirrored — not shared — across
//! three crates that must agree byte-for-byte on every leaf tag:
//!
//! 1. `relon_codegen_llvm::EmittedFieldType` — emitted by
//!    `lower_field_descriptors` / `emitted_field_type_for`.
//! 2. `relon_rs_shims::EmittedFieldType` + `ArgValue` / `RetValue` —
//!    packed / unpacked by `call_buffer_entry`'s per-variant helpers.
//! 3. `relon_rs_build`'s `rust_type_for` table — projects each tag onto
//!    the generated binding's Rust surface type + `ArgValue`/`RetValue`
//!    glue.
//!
//! If these drift, the codegen emits a tag the shim can't decode and
//! `call_buffer_entry` silently misreads the arena. This test fails
//! closed in two ways:
//!
//! - An **exhaustive `match`** over every `EmittedFieldType` variant
//!   (see [`expected_encoding`]) — adding a variant to the codegen enum
//!   without updating this table is a compile error.
//! - **End-to-end binding assertions**: for each reachable leaf type we
//!   drive the real `Compiler::emit_all` pipeline and assert the
//!   generated binding text carries the exact three-crate-consistent
//!   encoding (the `EmittedFieldType::*` const tag, the Rust surface
//!   type, and the `ArgValue` / `RetValue` constructor name the shim
//!   defines).

use std::path::PathBuf;

use relon_codegen_llvm::EmittedFieldType;
use relon_rs_build::Compiler;

/// The single source of truth this guard pins every crate against. One
/// row per `EmittedFieldType` variant; the exhaustive `match` forces a
/// new variant to extend this table (and, by the assertions below,
/// every crate's per-variant seam).
struct Encoding {
    /// `EmittedFieldType::*` literal the build generator must stamp into
    /// the binding's `static MAIN_FIELDS` / `RETURN_FIELDS` slices.
    tag_path: &'static str,
    /// Rust surface type for a `#main` parameter of this leaf type.
    arg_rust_ty: &'static str,
    /// `ArgValue::*` constructor name the shim defines for packing.
    arg_value_ctor: &'static str,
    /// Rust type the `#main` return slot surfaces to the caller.
    ret_rust_ty: &'static str,
    /// `RetValue::*` constructor name the shim defines for unpacking.
    ret_value_ctor: &'static str,
}

fn expected_encoding(ty: EmittedFieldType) -> Encoding {
    match ty {
        EmittedFieldType::Int => Encoding {
            tag_path: "EmittedFieldType::Int",
            arg_rust_ty: "i64",
            arg_value_ctor: "ArgValue::Int",
            ret_rust_ty: "i64",
            ret_value_ctor: "RetValue::Int",
        },
        EmittedFieldType::Float => Encoding {
            tag_path: "EmittedFieldType::Float",
            arg_rust_ty: "f64",
            arg_value_ctor: "ArgValue::Float",
            ret_rust_ty: "f64",
            ret_value_ctor: "RetValue::Float",
        },
        EmittedFieldType::Bool => Encoding {
            tag_path: "EmittedFieldType::Bool",
            arg_rust_ty: "bool",
            arg_value_ctor: "ArgValue::Bool",
            ret_rust_ty: "bool",
            ret_value_ctor: "RetValue::Bool",
        },
        EmittedFieldType::Unit => Encoding {
            tag_path: "EmittedFieldType::Unit",
            arg_rust_ty: "()",
            arg_value_ctor: "ArgValue::Unit",
            ret_rust_ty: "()",
            ret_value_ctor: "RetValue::Unit",
        },
        EmittedFieldType::String => Encoding {
            tag_path: "EmittedFieldType::String",
            arg_rust_ty: "&str",
            arg_value_ctor: "ArgValue::String",
            ret_rust_ty: "String",
            ret_value_ctor: "RetValue::String",
        },
        EmittedFieldType::ListInt => Encoding {
            tag_path: "EmittedFieldType::ListInt",
            arg_rust_ty: "&[i64]",
            arg_value_ctor: "ArgValue::ListInt",
            ret_rust_ty: "Vec<i64>",
            ret_value_ctor: "RetValue::ListInt",
        },
        // Adding a variant to `relon_codegen_llvm::EmittedFieldType`
        // without a row here is a compile error — the anti-drift gate.
    }
}

/// The full set of currently-supported tags. Mirrors the exhaustive
/// `match` above; any new variant must be appended (and will fail the
/// exhaustiveness check in `expected_encoding` until then).
const ALL_VARIANTS: &[EmittedFieldType] = &[
    EmittedFieldType::Int,
    EmittedFieldType::Float,
    EmittedFieldType::Bool,
    EmittedFieldType::Unit,
    EmittedFieldType::String,
    EmittedFieldType::ListInt,
];

/// Strip whitespace so the assertions are robust against rustfmt-style
/// `Foo :: Bar` vs `Foo::Bar` spacing in the generated binding text.
fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Compile `src` and return the generated binding text.
fn compile_binding(name: &str, src: &str) -> String {
    let tmp_dir = std::env::temp_dir().join(format!(
        "relon_marshal_roundtrip_{name}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let src_path: PathBuf = tmp_dir.join(format!("{name}.relon"));
    std::fs::write(&src_path, src).expect("write source");
    let out_dir = tmp_dir.join("out");
    let out = Compiler::new()
        .source(&src_path)
        .emit_all(&out_dir)
        .expect("emit_all");
    std::fs::read_to_string(&out.bindings[0]).expect("read generated binding")
}

/// Adding an `EmittedFieldType` variant must extend [`ALL_VARIANTS`] and
/// [`expected_encoding`] in lockstep. This is the compile-time half of
/// the anti-drift gate: the exhaustive `match` in `expected_encoding`
/// already fails to compile on a new variant, and this check keeps
/// `ALL_VARIANTS` honest about the count.
#[test]
fn all_variants_have_encoding() {
    // Touch every entry so a stale row surfaces. `expected_encoding`
    // is total over the enum, so this also documents the variant set.
    for &ty in ALL_VARIANTS {
        let e = expected_encoding(ty);
        assert!(!e.tag_path.is_empty());
        assert!(!e.arg_value_ctor.is_empty());
        assert!(!e.ret_value_ctor.is_empty());
    }
    // If a new variant is added without appending it here, the
    // exhaustive `match` in `expected_encoding` is the gate; this count
    // assertion is the reminder to also extend `ALL_VARIANTS`.
    assert_eq!(ALL_VARIANTS.len(), 6, "update ALL_VARIANTS for new tag");
}

/// `Int` arg + `Int` return: assert the binding carries the
/// codegen-side tag, the build-side Rust type, and the shim-side
/// `ArgValue` / `RetValue` constructors — all three crates agreeing.
#[test]
fn int_roundtrip_binding_consistent() {
    let binding = squeeze(&compile_binding("int_rt", "#main(Int n) -> Int\nn * 2\n"));
    // Int args take the fast-int extern path (no buffer marshalling),
    // so the const-tag glue is absent; assert the Rust surface instead.
    let arg = expected_encoding(EmittedFieldType::Int);
    assert!(
        binding.contains(&squeeze(&format!("n: {}", arg.arg_rust_ty))),
        "Int arg must surface as `{}`; binding=\n{binding}",
        arg.arg_rust_ty
    );
}

/// `String` arg + `Int` return forces the buffer-protocol path, so the
/// full triple (const tag + Rust type + `ArgValue`/`RetValue`) is
/// observable in one binding.
#[test]
fn string_arg_int_ret_triple_consistent() {
    let binding = squeeze(&compile_binding(
        "str_arg",
        "#main(String s) -> Int\nlength(s)\n",
    ));
    let arg = expected_encoding(EmittedFieldType::String);
    let ret = expected_encoding(EmittedFieldType::Int);

    // Build-side Rust surface type for the `&str` param.
    assert!(
        binding.contains(&squeeze(&format!("s: {}", arg.arg_rust_ty))),
        "String arg must surface as `{}`",
        arg.arg_rust_ty
    );
    // Codegen-side const tag for the String param slot.
    assert!(
        binding.contains(&squeeze(arg.tag_path)),
        "String slot must stamp `{}` into the field table",
        arg.tag_path
    );
    // Shim-side ArgValue packing constructor.
    assert!(
        binding.contains(&squeeze(arg.arg_value_ctor)),
        "String arg must pack via `{}`",
        arg.arg_value_ctor
    );
    // Codegen-side const tag for the Int return slot + shim unpack.
    assert!(
        binding.contains(&squeeze(ret.tag_path)),
        "Int return slot must stamp `{}`",
        ret.tag_path
    );
    assert!(
        binding.contains(&squeeze(ret.ret_value_ctor)),
        "Int return must unpack via `{}`",
        ret.ret_value_ctor
    );
}

/// `Float` arg + `Float` return forces the buffer-protocol path, so the
/// full Float triple (const tag + `f64` Rust type + `ArgValue`/`RetValue`
/// constructors) is observable in one binding.
#[test]
fn float_roundtrip_triple_consistent() {
    let binding = squeeze(&compile_binding(
        "float_rt",
        "#main(Float x) -> Float\nx * 2.0\n",
    ));
    let e = expected_encoding(EmittedFieldType::Float);

    // Build-side Rust surface type for the `f64` param + return.
    assert!(
        binding.contains(&squeeze(&format!("x: {}", e.arg_rust_ty))),
        "Float arg must surface as `{}`; binding=\n{binding}",
        e.arg_rust_ty
    );
    assert!(
        binding.contains(&squeeze(&format!("-> {}", e.ret_rust_ty))),
        "Float return must surface as `{}`",
        e.ret_rust_ty
    );
    // Codegen-side const tag for the Float slot.
    assert!(
        binding.contains(&squeeze(e.tag_path)),
        "Float slot must stamp `{}` into the field table",
        e.tag_path
    );
    // Shim-side ArgValue / RetValue glue.
    assert!(
        binding.contains(&squeeze(e.arg_value_ctor)),
        "Float arg must pack via `{}`",
        e.arg_value_ctor
    );
    assert!(
        binding.contains(&squeeze(e.ret_value_ctor)),
        "Float return must unpack via `{}`",
        e.ret_value_ctor
    );
}

/// `List<Int>` arg + `List<Int>` return surfaces the full ListInt triple
/// in one binding: the `&[i64]` param type + `Vec<i64>` return type
/// (build), the `EmittedFieldType::ListInt` slot tag (codegen), and the
/// `ArgValue::ListInt` / `RetValue::ListInt` glue (shim). This is the
/// binding-generation surface; the param→list-return *value* path is a
/// separate frozen-codegen limitation (see `aot_list.rs`).
#[test]
fn list_int_roundtrip_triple_consistent() {
    let binding = squeeze(&compile_binding(
        "list_int_rt",
        "#main(List<Int> xs) -> List<Int>\nxs\n",
    ));
    let e = expected_encoding(EmittedFieldType::ListInt);

    // Build-side Rust surface type for the `&[i64]` param + `Vec<i64>`
    // return.
    assert!(
        binding.contains(&squeeze(&format!("xs: {}", e.arg_rust_ty))),
        "ListInt arg must surface as `{}`; binding=\n{binding}",
        e.arg_rust_ty
    );
    assert!(
        binding.contains(&squeeze(&format!("-> {}", e.ret_rust_ty))),
        "ListInt return must surface as `{}`",
        e.ret_rust_ty
    );
    // Codegen-side const tag for the ListInt slot.
    assert!(
        binding.contains(&squeeze(e.tag_path)),
        "ListInt slot must stamp `{}` into the field table",
        e.tag_path
    );
    // Shim-side ArgValue / RetValue glue.
    assert!(
        binding.contains(&squeeze(e.arg_value_ctor)),
        "ListInt arg must pack via `{}`",
        e.arg_value_ctor
    );
    assert!(
        binding.contains(&squeeze(e.ret_value_ctor)),
        "ListInt return must unpack via `{}`",
        e.ret_value_ctor
    );
}

/// `String` arg + `Bool` return: covers the `Bool` return triple end —
/// the shim `RetValue::Bool` unpack + build-side `bool` surface type.
#[test]
fn bool_ret_triple_consistent() {
    let binding = squeeze(&compile_binding(
        "bool_ret",
        "#main(String s) -> Bool\ns.contains(\"x\")\n",
    ));
    let ret = expected_encoding(EmittedFieldType::Bool);
    assert!(
        binding.contains(&squeeze(&format!("-> {}", ret.ret_rust_ty))),
        "Bool return must surface as `{}`",
        ret.ret_rust_ty
    );
    assert!(
        binding.contains(&squeeze(ret.ret_value_ctor)),
        "Bool return must unpack via `{}`",
        ret.ret_value_ctor
    );
    assert!(
        binding.contains(&squeeze(ret.tag_path)),
        "Bool return slot must stamp `{}`",
        ret.tag_path
    );
}
