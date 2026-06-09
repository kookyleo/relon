//! Property-based write→read round-trip over the binary handshake
//! buffer, exercising the recursive host reader landed in `a1660b62`
//! (`BufferReader::read_list_record` / `read_list_list` + the per-field
//! decode the cranelift / llvm evaluators drive through
//! `read_value_from_reader`).
//!
//! The generators build values of the **return-side target shapes** —
//! `List<Schema>` (fields covering every scalar + `List<scalar>`),
//! `List<List<scalar>>`, plus the scalar / String / `List<scalar>`
//! leaves — write each value into a `BufferBuilder` through the same
//! typed writers the host marshaller uses, read it back through the
//! matching readers, and assert the decoded `Value` equals the input.
//! Strings cover empty / multi-byte (CJK, via `\u{...}` escapes) / long
//! / alignment-edge lengths; lists cover 0 / 1 / many; ints cover `MIN`
//! / `MAX`.
//!
//! Every successful round-trip is *also* fed through
//! [`relon_eval_api::verifier::verify_record`] over the whole-buffer
//! region — a clean verify is asserted, so the verifier and the reader
//! agree on "this buffer is well-formed" for the entire generated value
//! space (proptest auto-shrinks any counterexample).
//!
//! This is the S0 round-trip gate: it does **not** exercise the
//! compiled backends (no codegen output exists for these return shapes
//! yet — see the S0 load-bearing-wall proof). It validates the host
//! reader / writer pair and the verifier in isolation.

use std::sync::Arc;

use ordered_float::OrderedFloat;
use proptest::prelude::*;

use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::value::{Value, ValueDict};
use relon_eval_api::verifier::{verify_record, Region};

// --- scalar element generators ---------------------------------------

fn int_strat() -> impl Strategy<Value = i64> {
    prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0i64), any::<i64>(),]
}

fn float_strat() -> impl Strategy<Value = f64> {
    // Avoid NaN — `OrderedFloat` equality treats NaN specially and the
    // buffer round-trip is bit-preserving, but proptest's `==` on the
    // decoded `Value::Float` would otherwise need NaN-aware handling.
    prop_oneof![
        Just(0.0f64),
        Just(-0.0f64),
        Just(f64::MIN),
        Just(f64::MAX),
        (-1e9f64..1e9f64),
    ]
}

/// Strings covering the alignment / encoding edges the brief calls out:
/// empty, multi-byte CJK, long, and odd/aligned lengths.
fn string_strat() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("a".to_string()),
        Just("ab".to_string()),
        Just("abc".to_string()),
        Just("abcd".to_string()),
        // Multi-byte UTF-8 (3-byte CJK codepoints) via escapes so the
        // source file stays ASCII while the payload still exercises the
        // non-1-byte-per-char alignment edges.
        Just("\u{4e2d}".to_string()),
        Just("\u{4e2d}\u{6587}\u{5b57}\u{7b26}\u{4e32}".to_string()),
        Just("x".repeat(257)),
        "[a-z]{0,40}".prop_map(|s| s),
        // Mixed ASCII + multi-byte so the byte length is not a multiple
        // of the char count.
        "[a-z]{0,12}".prop_map(|s| {
            // Append a multi-byte codepoint half the time to break the
            // byte-len == char-len assumption.
            if s.len() % 2 == 0 {
                format!("{s}\u{4e2d}")
            } else {
                s
            }
        }),
    ]
}

// --- inner-list (List<scalar>) generators ----------------------------

fn list_int_strat() -> impl Strategy<Value = Vec<i64>> {
    prop::collection::vec(int_strat(), 0..6)
}

fn list_float_strat() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(float_strat(), 0..6)
}

fn list_bool_strat() -> impl Strategy<Value = Vec<bool>> {
    prop::collection::vec(any::<bool>(), 0..6)
}

fn list_string_strat() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(string_strat(), 0..5)
}

// --- helpers ----------------------------------------------------------

fn field(name: &str, ty: TypeRepr) -> Field {
    Field {
        name: name.into(),
        ty,
        default: None,
    }
}

fn list(inner: TypeRepr) -> TypeRepr {
    TypeRepr::List {
        element: Box::new(inner),
    }
}

/// Wrap a single field + value into a one-field schema, write it, read
/// it back, assert equality, and run the verifier clean. The single
/// shared helper keeps each property body to "generate → call".
fn roundtrip_one(field: Field, value: Value) {
    let schema = Schema {
        name: "Root".into(),
        generics: vec![],
        is_tuple: false,
        fields: vec![field.clone()],
    };
    let layout = SchemaLayout::offsets_for(&schema).expect("layout");
    let mut builder = BufferBuilder::new(&layout, &schema.fields);
    write_field(&mut builder, &field, &value);
    let bytes = builder.finish();

    // Verifier must pass on the whole-buffer region.
    let region = Region::new(0, bytes.len()).expect("region");
    verify_record(&bytes, &layout, &schema.fields, 0, region)
        .unwrap_or_else(|e| panic!("verifier rejected a legal buffer: {e}\n value={value:?}"));

    let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
    let decoded = read_field(&reader, &field);
    assert_eq!(
        decoded, value,
        "round-trip mismatch for field `{}`",
        field.name
    );
}

/// Write `value` into `builder` for `field` using the typed buffer
/// writers — mirrors `write_value_into_builder` in the evaluators but
/// also reaches the `List<Schema>` / `List<List<_>>` writers the input
/// path does not exercise (those are return-only shapes here).
fn write_field(builder: &mut BufferBuilder<'_>, field: &Field, value: &Value) {
    match (&field.ty, value) {
        (TypeRepr::Int, Value::Int(v)) => builder.write_int(&field.name, *v).unwrap(),
        (TypeRepr::Float, Value::Float(v)) => {
            builder.write_float(&field.name, v.into_inner()).unwrap()
        }
        (TypeRepr::Bool, Value::Bool(v)) => builder.write_bool(&field.name, *v).unwrap(),
        (TypeRepr::String, Value::String(s)) => {
            builder.write_string(&field.name, s.as_str()).unwrap()
        }
        (TypeRepr::List { element }, Value::List(items)) => {
            write_list_field(builder, &field.name, element, items)
        }
        other => panic!("write_field: unhandled ({:?}, value)", other.0),
    }
}

fn write_list_field(
    builder: &mut BufferBuilder<'_>,
    name: &str,
    element: &TypeRepr,
    items: &[Value],
) {
    match element {
        TypeRepr::Int => {
            let v: Vec<i64> = items
                .iter()
                .map(|x| match x {
                    Value::Int(i) => *i,
                    _ => panic!("expected Int element"),
                })
                .collect();
            builder.write_list_int(name, &v).unwrap();
        }
        TypeRepr::Float => {
            let v: Vec<f64> = items
                .iter()
                .map(|x| match x {
                    Value::Float(f) => f.into_inner(),
                    _ => panic!("expected Float element"),
                })
                .collect();
            builder.write_list_float(name, &v).unwrap();
        }
        TypeRepr::Bool => {
            let v: Vec<bool> = items
                .iter()
                .map(|x| match x {
                    Value::Bool(b) => *b,
                    _ => panic!("expected Bool element"),
                })
                .collect();
            builder.write_list_bool(name, &v).unwrap();
        }
        TypeRepr::String => {
            let v: Vec<&str> = items
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.as_str(),
                    _ => panic!("expected String element"),
                })
                .collect();
            builder.write_list_string(name, &v).unwrap();
        }
        TypeRepr::Schema { schema } => {
            let elem_layout = SchemaLayout::offsets_for(schema).expect("elem layout");
            let mut writer = builder
                .list_record_writer(name, &elem_layout, schema)
                .expect("list_record_writer");
            for it in items {
                let Value::Dict(dict) = it else {
                    panic!("expected branded Dict element");
                };
                let mut child = writer.start_entry();
                for f in &schema.fields {
                    let fv = dict.map.get(f.name.as_str()).expect("dict field present");
                    write_field(&mut child, f, fv);
                }
                writer.finish_entry(builder, child).expect("finish_entry");
            }
            builder
                .finish_list_record(writer)
                .expect("finish_list_record");
        }
        TypeRepr::List { element: inner } => {
            // List<List<scalar>>: drive `write_nested_scalar_list`.
            relon_eval_api::buffer::write_nested_scalar_list(builder, name, inner, items)
                .expect("write_nested_scalar_list");
        }
        other => panic!("write_list_field: unhandled element {other:?}"),
    }
}

/// Read `field` back as a `Value`, mirroring `read_value_from_reader`
/// in the evaluators (the a1660b62 host reader).
fn read_field(reader: &BufferReader<'_>, field: &Field) -> Value {
    match &field.ty {
        TypeRepr::Int => Value::Int(reader.read_int(&field.name).unwrap()),
        TypeRepr::Float => Value::Float(OrderedFloat(reader.read_float(&field.name).unwrap())),
        TypeRepr::Bool => Value::Bool(reader.read_bool(&field.name).unwrap()),
        TypeRepr::String => Value::String(reader.read_string(&field.name).unwrap().into()),
        TypeRepr::List { element } => read_list_field(reader, &field.name, element),
        other => panic!("read_field: unhandled {other:?}"),
    }
}

fn read_list_field(reader: &BufferReader<'_>, name: &str, element: &TypeRepr) -> Value {
    match element {
        TypeRepr::Int => Value::List(Arc::new(
            reader
                .read_list_int(name)
                .unwrap()
                .into_iter()
                .map(Value::Int)
                .collect(),
        )),
        TypeRepr::Float => Value::List(Arc::new(
            reader
                .read_list_float(name)
                .unwrap()
                .into_iter()
                .map(|f| Value::Float(OrderedFloat(f)))
                .collect(),
        )),
        TypeRepr::Bool => Value::List(Arc::new(
            reader
                .read_list_bool(name)
                .unwrap()
                .into_iter()
                .map(Value::Bool)
                .collect(),
        )),
        TypeRepr::String => Value::List(Arc::new(
            reader
                .read_list_string(name)
                .unwrap()
                .into_iter()
                .map(|s| Value::String(s.into()))
                .collect(),
        )),
        TypeRepr::Schema { schema } => {
            let elem_layout = SchemaLayout::offsets_for(schema).expect("elem layout");
            let subs = reader
                .read_list_record(name, &elem_layout, schema)
                .expect("read_list_record");
            let mut items = Vec::with_capacity(subs.len());
            for sub in &subs {
                let mut entries: Vec<(String, Value)> = Vec::new();
                for f in &schema.fields {
                    entries.push((f.name.clone(), read_field(sub, f)));
                }
                items.push(Value::branded_dict(entries, Some(schema.name.clone())));
            }
            Value::List(Arc::new(items))
        }
        TypeRepr::List { .. } => Value::List(Arc::new(
            reader
                .read_list_list(name)
                .expect("read_list_list")
                .into_iter()
                .map(|row| Value::List(Arc::new(row)))
                .collect(),
        )),
        other => panic!("read_list_field: unhandled element {other:?}"),
    }
}

// --- List<Schema> element generator ----------------------------------

/// A schema whose fields cover every scalar + each `List<scalar>` —
/// the "fields contain Int/Float/Bool/String/List" shape the brief
/// asks for in the `List<Schema>` generator.
fn elem_schema() -> Schema {
    Schema {
        name: "Elem".into(),
        generics: vec![],
        is_tuple: false,
        fields: vec![
            field("i", TypeRepr::Int),
            field("f", TypeRepr::Float),
            field("b", TypeRepr::Bool),
            field("s", TypeRepr::String),
            field("li", list(TypeRepr::Int)),
            field("ls", list(TypeRepr::String)),
        ],
    }
}

fn elem_value_strat() -> impl Strategy<Value = Value> {
    (
        int_strat(),
        float_strat(),
        any::<bool>(),
        string_strat(),
        list_int_strat(),
        list_string_strat(),
    )
        .prop_map(|(i, f, b, s, li, ls)| {
            let entries: Vec<(String, Value)> = vec![
                ("i".into(), Value::Int(i)),
                ("f".into(), Value::Float(OrderedFloat(f))),
                ("b".into(), Value::Bool(b)),
                ("s".into(), Value::String(s.into())),
                (
                    "li".into(),
                    Value::List(Arc::new(li.into_iter().map(Value::Int).collect())),
                ),
                (
                    "ls".into(),
                    Value::List(Arc::new(
                        ls.into_iter().map(|x| Value::String(x.into())).collect(),
                    )),
                ),
            ];
            Value::branded_dict(entries, Some("Elem".to_string()))
        })
}

// --- List<List<scalar>> generator ------------------------------------

fn list_list_int_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(list_int_strat(), 0..4).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| Value::List(Arc::new(r.into_iter().map(Value::Int).collect())))
                .collect(),
        ))
    })
}

fn list_list_bool_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(list_bool_strat(), 0..4).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| Value::List(Arc::new(r.into_iter().map(Value::Bool).collect())))
                .collect(),
        ))
    })
}

fn list_list_float_strat() -> impl Strategy<Value = Value> {
    prop::collection::vec(list_float_strat(), 0..4).prop_map(|rows| {
        Value::List(Arc::new(
            rows.into_iter()
                .map(|r| {
                    Value::List(Arc::new(
                        r.into_iter()
                            .map(|f| Value::Float(OrderedFloat(f)))
                            .collect(),
                    ))
                })
                .collect(),
        ))
    })
}

// Suppress an unused-import warning for `ValueDict` on toolchains that
// don't need it for inference; the type is part of the value surface we
// round-trip.
#[allow(dead_code)]
fn _value_dict_marker(_: &ValueDict) {}

proptest! {
    // miri runs ~10-100x slower; keep every proptest function exercising
    // the buffer/verifier unsafe paths under miri, but shrink the random
    // case count so the job stays well under its timeout.
    #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 8 } else { 256 }))]

    #[test]
    fn roundtrip_scalar_int(v in int_strat()) {
        roundtrip_one(field("i", TypeRepr::Int), Value::Int(v));
    }

    #[test]
    fn roundtrip_scalar_float(v in float_strat()) {
        roundtrip_one(field("f", TypeRepr::Float), Value::Float(OrderedFloat(v)));
    }

    #[test]
    fn roundtrip_scalar_bool(v in any::<bool>()) {
        roundtrip_one(field("b", TypeRepr::Bool), Value::Bool(v));
    }

    #[test]
    fn roundtrip_string(s in string_strat()) {
        roundtrip_one(field("s", TypeRepr::String), Value::String(s.into()));
    }

    #[test]
    fn roundtrip_list_int(v in list_int_strat()) {
        let val = Value::List(Arc::new(v.into_iter().map(Value::Int).collect()));
        roundtrip_one(field("li", list(TypeRepr::Int)), val);
    }

    #[test]
    fn roundtrip_list_float(v in list_float_strat()) {
        let val = Value::List(Arc::new(
            v.into_iter().map(|f| Value::Float(OrderedFloat(f))).collect(),
        ));
        roundtrip_one(field("lf", list(TypeRepr::Float)), val);
    }

    #[test]
    fn roundtrip_list_bool(v in list_bool_strat()) {
        let val = Value::List(Arc::new(v.into_iter().map(Value::Bool).collect()));
        roundtrip_one(field("lb", list(TypeRepr::Bool)), val);
    }

    #[test]
    fn roundtrip_list_string(v in list_string_strat()) {
        let val = Value::List(Arc::new(
            v.into_iter().map(|s| Value::String(s.into())).collect(),
        ));
        roundtrip_one(field("ls", list(TypeRepr::String)), val);
    }

    #[test]
    fn roundtrip_list_schema(items in prop::collection::vec(elem_value_strat(), 0..4)) {
        let val = Value::List(Arc::new(items));
        let fld = field(
            "rows",
            list(TypeRepr::Schema {
                schema: Box::new(elem_schema()),
            }),
        );
        roundtrip_one(fld, val);
    }

    #[test]
    fn roundtrip_list_list_int(val in list_list_int_strat()) {
        roundtrip_one(field("m", list(list(TypeRepr::Int))), val);
    }

    #[test]
    fn roundtrip_list_list_bool(val in list_list_bool_strat()) {
        roundtrip_one(field("m", list(list(TypeRepr::Bool))), val);
    }

    #[test]
    fn roundtrip_list_list_float(val in list_list_float_strat()) {
        roundtrip_one(field("m", list(list(TypeRepr::Float))), val);
    }
}

// --- F0 cross-region object generator (multi-region verifier) ---------
//
// The F1 target shape is `-> Dict { servers: List<Cfg>, n: Int }`: an
// object head built in `out_buf` whose `servers` field points at a
// parameter-sourced `List<Cfg>` whose header / entries / sub-records /
// String fields all live in `in_buf`. F0 ships no cap release, so we
// hand-assemble that cross-region arena (object head in `out`, field data
// in `in`, every pointer arena-absolute) and drive the multi-region
// verifier over the whole value space. Legal arenas must verify clean;
// a corrupted pointer must be rejected loudly (never a panic / over-read).

use relon_eval_api::layout::OffsetTable;
use relon_eval_api::verifier::{verify_record_multi, MultiRegion};

/// `Cfg { name: String, port: Int }` — the cross-region element schema.
fn xr_cfg_schema() -> Schema {
    Schema {
        name: "Cfg".into(),
        generics: vec![],
        is_tuple: false,
        fields: vec![
            field("name", TypeRepr::String),
            field("port", TypeRepr::Int),
        ],
    }
}

/// `Out { servers: List<Cfg>, n: Int }` — the cross-region object schema.
fn xr_outer_schema() -> Schema {
    Schema {
        name: "Out".into(),
        generics: vec![],
        is_tuple: false,
        fields: vec![
            field(
                "servers",
                list(TypeRepr::Schema {
                    schema: Box::new(xr_cfg_schema()),
                }),
            ),
            field("n", TypeRepr::Int),
        ],
    }
}

fn le_u32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

/// Assemble a cross-region arena for `Out { servers: [Cfg{name,port}..], n }`:
/// the `in` region holds the whole `List<Cfg>` graph with arena-absolute
/// pointers; the `out` region holds the object head whose `servers` slot
/// points (cross-region) at the `in`-region list header. Returns
/// `(arena, multi, outer_layout, outer_schema, out_record_base, pointer_slot_abs_offsets)`.
/// `pointer_slot_abs_offsets` lists every arena-absolute byte position that
/// holds a pointer the verifier follows, so a proptest can corrupt one.
#[allow(clippy::type_complexity)]
fn build_xr_arena(
    servers: &[(String, i64)],
    n: i64,
) -> (Vec<u8>, MultiRegion, OffsetTable, Schema, usize, Vec<usize>) {
    let cfg = xr_cfg_schema();
    let cfg_layout = SchemaLayout::offsets_for(&cfg).expect("cfg layout");
    let outer = xr_outer_schema();
    let outer_layout = SchemaLayout::offsets_for(&outer).expect("outer layout");

    let name_fo = cfg_layout.fields.iter().find(|f| f.name == "name").unwrap();
    let port_fo = cfg_layout.fields.iter().find(|f| f.name == "port").unwrap();

    // Region-local layout of the `in` buffer:
    //   [list header len][off_0..off_{k-1}][cfg_0 ..][cfg_{k-1}][name strings..]
    let k = servers.len();
    let header_rel = 0usize;
    let entries_rel = header_rel + 4;
    let cfgs_rel = entries_rel + k * 4;
    let cfg_rel = |i: usize| cfgs_rel + i * cfg_layout.root_size;
    let names_rel = cfgs_rel + k * cfg_layout.root_size;
    // Compute each name's region-local offset.
    let mut name_rel = Vec::with_capacity(k);
    let mut cursor = names_rel;
    for (s, _) in servers {
        name_rel.push(cursor);
        cursor += 4 + s.len();
    }
    let in_len = cursor.max(4);

    // Region geometry (disjoint, padded windows).
    let const_len = 16usize;
    let in_start = const_len;
    let in_end = in_start + in_len;
    let out_start = in_end + 8;
    let out_len = outer_layout.root_size;
    let out_end = out_start + out_len;
    let scratch_start = out_end + 8;
    let arena_size = scratch_start + 16;
    let mut arena = vec![0u8; arena_size];

    let mut ptr_slots: Vec<usize> = Vec::new();
    let put32 = |arena: &mut [u8], abs: usize, v: u32| {
        arena[abs..abs + 4].copy_from_slice(&le_u32(v));
    };
    let put64 = |arena: &mut [u8], abs: usize, v: i64| {
        arena[abs..abs + 8].copy_from_slice(&v.to_le_bytes());
    };

    // List header + entry pointers (arena-absolute).
    put32(&mut arena, in_start + header_rel, k as u32);
    for i in 0..k {
        let slot = in_start + entries_rel + i * 4;
        put32(&mut arena, slot, (in_start + cfg_rel(i)) as u32);
        ptr_slots.push(slot);
    }
    // Each Cfg sub-record: name pointer (arena-absolute) + inline port.
    for (i, (s, port)) in servers.iter().enumerate() {
        let name_slot = in_start + cfg_rel(i) + name_fo.offset;
        put32(&mut arena, name_slot, (in_start + name_rel[i]) as u32);
        ptr_slots.push(name_slot);
        put64(&mut arena, in_start + cfg_rel(i) + port_fo.offset, *port);
        // String record [len][utf8].
        let rec = in_start + name_rel[i];
        put32(&mut arena, rec, s.len() as u32);
        arena[rec + 4..rec + 4 + s.len()].copy_from_slice(s.as_bytes());
    }

    // Object head in `out`: `servers` -> list header in `in` (cross-region).
    let servers_fo = outer_layout
        .fields
        .iter()
        .find(|f| f.name == "servers")
        .unwrap();
    let n_fo = outer_layout.fields.iter().find(|f| f.name == "n").unwrap();
    let servers_slot = out_start + servers_fo.offset;
    put32(&mut arena, servers_slot, (in_start + header_rel) as u32);
    ptr_slots.push(servers_slot);
    put64(&mut arena, out_start + n_fo.offset, n);

    let multi = MultiRegion::new(
        (0, const_len),
        (in_start, in_end),
        (out_start, out_end),
        (scratch_start, arena_size),
    )
    .expect("multi region");

    (arena, multi, outer_layout, outer, out_start, ptr_slots)
}

fn xr_servers_strat() -> impl Strategy<Value = Vec<(String, i64)>> {
    prop::collection::vec((string_strat(), int_strat()), 0..4)
}

proptest! {
    // miri runs ~10-100x slower; keep every proptest function exercising
    // the buffer/verifier unsafe paths under miri, but shrink the random
    // case count so the job stays well under its timeout.
    #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 8 } else { 256 }))]

    /// A legal cross-region `Dict { servers: List<Cfg>, n }` arena must
    /// verify clean under the multi-region map for the whole value space.
    #[test]
    fn xr_legal_object_verifies(servers in xr_servers_strat(), n in int_strat()) {
        let (arena, multi, layout, schema, base, _slots) = build_xr_arena(&servers, n);
        verify_record_multi(&arena, &layout, &schema.fields, base, multi)
            .map_err(|e| TestCaseError::fail(format!("legal cross-region object rejected: {e}")))?;
    }

    /// Corrupting any one followed pointer to a far out-of-arena offset
    /// must make the multi-region verifier reject loudly — never panic,
    /// never over-read.
    #[test]
    fn xr_corrupt_pointer_rejected(
        servers in xr_servers_strat(),
        n in int_strat(),
        which in any::<prop::sample::Index>(),
    ) {
        let (mut arena, multi, layout, schema, base, slots) = build_xr_arena(&servers, n);
        prop_assume!(!slots.is_empty());
        let idx = which.index(slots.len());
        let slot = slots[idx];
        let bogus = (arena.len() as u32) + 4096;
        arena[slot..slot + 4].copy_from_slice(&le_u32(bogus));
        let res = verify_record_multi(&arena, &layout, &schema.fields, base, multi);
        prop_assert!(res.is_err(), "a corrupt cross-region pointer must be rejected, got Ok");
    }
}
