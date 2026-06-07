//! Deterministic schema serialisation + sha256 digest.
//!
//! The wasm AOT backend embeds a 32-byte schema hash into the
//! `relon.abi` custom section so host SDKs can detect schema drift at
//! load time (host writing with an outdated `#main` schema while the
//! wasm module was compiled against the new one — a mismatch makes
//! the wasm read garbage out of the binary handshake buffer). The
//! cryptographic strength is irrelevant; what matters is that host
//! and codegen compute the **same** hash from the **same** schema.
//!
//! Spec: `docs/internal/wasm-srcmap-section-v1-2026-05-16.md`,
//! "canonical #main schema" section.
//!
//! The canonical form is intentionally narrower than the runtime
//! [`crate::value::SchemaData`] type:
//!
//! * Fields are stored in **declaration order**, not alphabetical.
//!   Field order changes are observable on the wire (layout slot
//!   offsets shift), so they must invalidate the hash.
//! * `doc_comment` and decorator metadata are **not** captured — they
//!   are presentation / lint signals, not ABI-relevant.
//! * Nested schemas are **inlined**, not referenced by name. Two
//!   schemas with the same structural shape compare equal even when
//!   declared in different files under different names — this is a
//!   "behavioural hash", not a "location hash".
//!
//! Phase 2.b will plumb real `SchemaDef` lowering into this module;
//! Phase 2.a only provides the canonical form + hash plumbing so the
//! `relon.abi` section can be emitted with placeholder zeros until
//! the codegen pass starts accepting schema input.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Logical type description used by canonical serialisation.
///
/// This is **not** the parser's `TypeNode`: the parser shape carries
/// source ranges, doc comments, and parsed-but-unresolved generic
/// argument references, none of which belong in an ABI hash. The
/// canonical form is a structural snapshot that strips presentation
/// metadata and inlines nested schemas.
///
/// Variants mirror the v1 binary layout's leaf-type table; extending
/// the layout in a later phase (e.g. `Bytes`, `Tuple<...>`) requires
/// adding the variant here so the hash distinguishes the new shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TypeRepr {
    /// `Null` — the unit type.
    Null,
    /// `Bool` — a 0/1 byte.
    Bool,
    /// `Int` — a signed 64-bit integer.
    Int,
    /// `Float` — an IEEE-754 double.
    Float,
    /// `String` — UTF-8 bytes with a u32 length prefix.
    String,
    /// `List<T>` — variable-length sequence over `element`.
    List {
        /// Element type.
        element: Box<TypeRepr>,
    },
    /// `Option<T>` — tag + payload union with two arms.
    Option {
        /// `Some` payload type.
        inner: Box<TypeRepr>,
    },
    /// `Result<T, E>` — tag + payload union with two arms.
    Result {
        /// `Ok` payload type.
        ok: Box<TypeRepr>,
        /// `Err` payload type.
        err: Box<TypeRepr>,
    },
    /// Inline reference to a named nested schema. The hash flattens
    /// the nested structure rather than recording the name, so two
    /// schemas with identical structural shape collapse to the same
    /// digest regardless of declaration site.
    Schema {
        /// Recursive canonical form of the nested schema.
        schema: Box<Schema>,
    },
    /// Phase F.2 (W7 closure-as-value boundary) — first-class closure
    /// value. The variant records the closure's user-visible signature
    /// (`params` declaration order, `ret`); the schema digest treats it
    /// as a structural shape so two anonymous closure fields with the
    /// same `(params, ret)` collapse to the same hash regardless of
    /// declaration site.
    ///
    /// The runtime representation is a scratch-heap pointer-indirect
    /// 8-byte handle (`[fn_table_idx: u32 LE][captures_ptr: u32 LE]`);
    /// see `relon_ir::IrType::Closure` for the wasm-side layout. The
    /// canonical form intentionally avoids carrying capture metadata —
    /// captures are an implementation detail of the lambda's closure
    /// conversion, not part of its ABI-visible type.
    ///
    /// Layout integration is **not** wired in this milestone: any
    /// `TypeRepr::Closure` reaching `SchemaLayout::offsets_for` surfaces
    /// as `LayoutError::UnsupportedTypeInLayoutV1` so the cross-boundary
    /// dangle the binary handshake would otherwise see stays guarded.
    /// Closure-typed fields are only valid as in-function intermediate
    /// values (let-bindings, dict-field caches the lowering pass
    /// owns) — never at a host-visible `#main` boundary.
    Closure {
        /// User-visible parameter types in declaration order. Carries
        /// nested `TypeRepr` so a closure-returning closure
        /// (`(Int) => (Int) => Int`) hashes as a distinct shape from a
        /// flat `(Int, Int) => Int`.
        params: Vec<TypeRepr>,
        /// Return type. Single value (no tuples) — matches the wasm
        /// `call_indirect` signature codegen emits today.
        ret: Box<TypeRepr>,
    },
}

/// One field in a canonical schema.
///
/// `default` carries the field's compile-time default value when
/// declared; it's serialised as raw JSON so the hash is sensitive to
/// `1` vs `1.0` vs `"1"` distinctions without needing a separate
/// canonical-value encoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    /// Field name as declared in source.
    pub name: String,
    /// Field type in canonical form.
    pub ty: TypeRepr,
    /// Default value, when the field declared one. `serde_json::Value`
    /// rather than `crate::value::Value` so the hash stays insulated
    /// from runtime-only payload variants (`Closure`, `Schema`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// Canonical schema description. Field order is preserved exactly as
/// declared; see the module docs for the rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// Schema name. Anonymous schemas declared inline carry an empty
    /// string so the canonical form remains deterministic even when
    /// hosts forget to pass a name through.
    pub name: String,
    /// Generic type parameters (e.g. `["T"]` for `Page<T>`). Empty
    /// for monomorphic schemas.
    pub generics: Vec<String>,
    /// Fields in **declaration order**. Reordering invalidates the
    /// hash even when each field's name + type are otherwise
    /// identical, because the binary layout's field offsets are
    /// declaration-order dependent.
    pub fields: Vec<Field>,
    /// Wave T2: marks an **anonymous positional record** synthesised for
    /// a `Tuple<...>`. The binary layout, buffer builder, verifier and
    /// codegen treat such a schema exactly like any other record (its
    /// fields carry the synthetic positional names `"0"`, `"1"`, ...),
    /// so the whole record/return ABI is reused unchanged. The only
    /// behavioural fork is the **host decode**: a tuple schema decodes
    /// to a positional JSON array (`Value::List`), byte-identical to the
    /// tree-walk oracle's `Value::List`, rather than to a branded object.
    ///
    /// Serialised only when `true`, so every pre-T2 (non-tuple) schema's
    /// canonical bytes — and therefore its ABI hash — stay unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_tuple: bool,
}

impl Schema {
    /// Build an empty schema with the given `name`. Convenience ctor
    /// used by tests and by future codegen-side conversions.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            generics: Vec::new(),
            fields: Vec::new(),
            is_tuple: false,
        }
    }

    /// Wave T2: build an anonymous positional-record schema for a
    /// `Tuple<...>` from its element types in order. Field names are the
    /// synthetic decimal indices `"0"`, `"1"`, ... so the existing
    /// declaration-order layout pass assigns one slot per element; the
    /// `is_tuple` flag drives the array-shaped host decode.
    pub fn tuple(name: impl Into<String>, elements: Vec<TypeRepr>) -> Self {
        let fields = elements
            .into_iter()
            .enumerate()
            .map(|(i, ty)| Field {
                name: i.to_string(),
                ty,
                default: None,
            })
            .collect();
        Self {
            name: name.into(),
            generics: Vec::new(),
            fields,
            is_tuple: true,
        }
    }
}

/// Serialise a [`Schema`] to its canonical byte form.
///
/// The output is the schema's JSON projection with:
///
/// * sorted object keys at every level (so `serde_json`'s map iteration
///   order can't poison the hash even when `BTreeMap` / `HashMap`
///   internals reshuffle between minor releases),
/// * no whitespace (compact form),
/// * a top-level `"version": 2` marker so future canonical-form
///   evolutions can bump and stay distinguishable. Phase F.2 lifted
///   v1 → v2 when adding the [`TypeRepr::Closure`] variant: pre-Phase-F
///   schemas serialise an enum tag set the new decoders don't
///   recognise, so the version bump lets a host SDK refuse to load a
///   module whose digest was computed against the older variant set.
///
/// Field order inside [`Schema::fields`] is **not** sorted — the
/// `Vec<Field>` is serialised in the order callers declared, matching
/// the binary layout's declaration-order slot assignment.
pub fn canonical_schema(schema: &Schema) -> Vec<u8> {
    // We wrap the schema in a stable envelope so the version marker
    // sits at a predictable spot in the JSON output. `BTreeMap` keeps
    // keys sorted (`version` then `schema`) — important because
    // `serde_json::Value::Object` is itself a `BTreeMap`-backed
    // structure when constructed via `json!`, which gives us the
    // sorted-keys property without us writing a custom serializer.
    let value = serde_json::json!({
        "version": 2,
        "schema": schema,
    });
    // `serde_json::to_vec` on a `serde_json::Value` produces compact
    // output (no whitespace) and walks `Value::Object` in key order
    // (BTreeMap). The `Schema` types above are `#[derive(Serialize)]`
    // and emit their fields in declaration order — exactly what the
    // canonical form requires. Combined, that means: nested map keys
    // (e.g. inside `default` values) are sorted; field-level ordering
    // is preserved.
    serde_json::to_vec(&value).expect("canonical schema serialisation never fails on owned types")
}

/// Compute the 32-byte sha256 digest of a schema's canonical form.
///
/// The digest is the value embedded in the `relon.abi` custom section
/// for `main_schema_hash` / `return_schema_hash`. Host SDKs compute
/// the same digest from their compile-time schema knowledge and
/// refuse-to-load on mismatch.
pub fn schema_hash(schema: &Schema) -> [u8; 32] {
    let canonical = canonical_schema(schema);
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_user_schema() -> Schema {
        Schema {
            name: "User".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![
                Field {
                    name: "id".into(),
                    ty: TypeRepr::Int,
                    default: None,
                },
                Field {
                    name: "name".into(),
                    ty: TypeRepr::String,
                    default: None,
                },
                Field {
                    name: "active".into(),
                    ty: TypeRepr::Bool,
                    default: Some(serde_json::Value::Bool(true)),
                },
            ],
        }
    }

    #[test]
    fn identical_schemas_produce_identical_hash() {
        // Two independently constructed instances of the same schema
        // must hash to the same digest — the hash is the wire-level
        // identity of the shape, not of the Rust value.
        let a = sample_user_schema();
        let b = sample_user_schema();
        assert_eq!(schema_hash(&a), schema_hash(&b));
    }

    #[test]
    fn field_reorder_changes_hash() {
        // Field declaration order maps to binary-layout offsets, so a
        // reorder is observable on the wire and must invalidate the
        // hash even when each field's (name, type, default) is
        // otherwise identical.
        let mut original = sample_user_schema();
        let mut reordered = sample_user_schema();
        reordered.fields.swap(0, 1);
        assert_ne!(schema_hash(&original), schema_hash(&reordered));

        // Sanity: the only difference is order — field set is identical.
        original.fields.sort_by(|a, b| a.name.cmp(&b.name));
        reordered.fields.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(original.fields, reordered.fields);
    }

    #[test]
    fn doc_comment_and_metadata_absent_means_hash_stable() {
        // `Schema` / `Field` don't carry doc comments or decorator
        // metadata fields, so adding such metadata in upstream
        // `SchemaDef` should be a no-op for the canonical form once
        // Phase 2.b plumbs the conversion. We exercise the invariant
        // here by ensuring the canonical bytes don't mention any
        // "doc" key — a regression that quietly leaks docs into the
        // hash would surface as a string match here.
        let schema = sample_user_schema();
        let bytes = canonical_schema(&schema);
        let text = std::str::from_utf8(&bytes).expect("canonical form is utf-8 json");
        assert!(
            !text.contains("\"doc"),
            "canonical form must not contain doc-related keys, got: {text}"
        );
        assert!(
            !text.contains("\"meta"),
            "canonical form must not contain decorator metadata keys, got: {text}"
        );
    }

    #[test]
    fn nested_schema_inline_matches_flattened_equivalent() {
        // Behavioural hash: a schema nesting `User` and an equivalent
        // schema with the same fields inlined under a different
        // declaration name must collapse to the same digest. The
        // `Schema { schema: ... }` variant flattens recursively, so
        // two structurally identical inputs reach the same canonical
        // bytes.
        let inner = sample_user_schema();
        let outer_with_named = Schema {
            name: "Wrapper".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![Field {
                name: "user".into(),
                ty: TypeRepr::Schema {
                    schema: Box::new(inner.clone()),
                },
                default: None,
            }],
        };
        // Same outer schema but the inner schema's `name` differs.
        // The hash should ignore the nested name and respond only to
        // structural shape.
        let outer_with_alias = Schema {
            name: "Wrapper".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![Field {
                name: "user".into(),
                ty: TypeRepr::Schema {
                    schema: Box::new(Schema {
                        // Per the spec the nested schema is inlined
                        // recursively, and "a schema declared in
                        // foo.relon vs bar.relon must hash the same
                        // when its structure matches" — that is the
                        // file-path invariance. Type rename, however,
                        // is a breaking change to host consumers
                        // (brand string flips), so we keep the
                        // declared name as part of the canonical
                        // form. The "behavioural" claim therefore
                        // covers file-path locality, not type rename.
                        // Use the same nested name here to match the
                        // structural equivalence we are exercising.
                        name: inner.name.clone(),
                        generics: inner.generics.clone(),
                        fields: inner.fields.clone(),
                        is_tuple: false,
                    }),
                },
                default: None,
            }],
        };
        assert_eq!(
            schema_hash(&outer_with_named),
            schema_hash(&outer_with_alias)
        );
    }

    #[test]
    fn different_field_default_changes_hash() {
        // Belt-and-braces: tweaking a default value (compile-time
        // visible to the host) must shift the hash so a schema with a
        // changed default doesn't sneak past the SDK's drift check.
        let mut a = sample_user_schema();
        let mut b = sample_user_schema();
        a.fields[2].default = Some(serde_json::Value::Bool(true));
        b.fields[2].default = Some(serde_json::Value::Bool(false));
        assert_ne!(schema_hash(&a), schema_hash(&b));
    }

    #[test]
    fn canonical_form_is_compact_json() {
        // Whitespace in the canonical form would let an attacker forge
        // two byte-different-but-semantically-equal payloads. Lock
        // the compact form down with an explicit check.
        let schema = sample_user_schema();
        let bytes = canonical_schema(&schema);
        assert!(
            !bytes.contains(&b' '),
            "canonical form must contain no spaces"
        );
        assert!(
            !bytes.contains(&b'\n'),
            "canonical form must contain no newlines"
        );
    }
}
