//! Native three-way coverage for compiled Option / Result returns.

use std::collections::HashMap;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

fn args(items: impl IntoIterator<Item = (&'static str, Value)>) -> HashMap<String, Value> {
    items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn result_ok(value: Value) -> Value {
    Value::variant_dict([("value", value)], "Ok".to_string(), "Result".to_string())
}

fn result_err(error: Value) -> Value {
    Value::variant_dict([("error", error)], "Err".to_string(), "Result".to_string())
}

fn run_tree_walk(src: &str, args: HashMap<String, Value>) -> Value {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    use std::sync::Arc;

    let node = parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let ctx = Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(Arc::clone(&ctx))
        .run_main(&Arc::new(relon_eval_api::scope::Scope::default()), args)
        .expect("tree-walk run_main")
}

fn assert_native_three_way(src: &str, args: HashMap<String, Value>, expected: Value) {
    let tree = run_tree_walk(src, args.clone());
    assert_eq!(tree, expected, "tree-walk result");

    let cranelift = AotEvaluator::from_source(src)
        .expect("cranelift compile")
        .run_main(args.clone())
        .expect("cranelift run_main");
    assert_eq!(cranelift, expected, "cranelift result");

    let llvm = LlvmAotEvaluator::from_source(src)
        .expect("llvm compile")
        .run_main(args)
        .expect("llvm run_main");
    assert_eq!(llvm, expected, "llvm result");
}

#[test]
fn option_some_int_return_compiles() {
    assert_native_three_way(
        "#main(Int n) -> Option<Int>\nOption.Some { value: n }",
        args([("n", Value::Int(7))]),
        Value::option_some(Value::Int(7)),
    );
}

#[test]
fn option_none_string_return_compiles() {
    assert_native_three_way(
        "#main() -> Option<String>\nOption.None {}",
        HashMap::new(),
        Value::option_none(),
    );
}

#[test]
fn result_ok_and_err_return_compile() {
    assert_native_three_way(
        "#main() -> Result<Int, String>\nResult.Ok { value: 42 }",
        HashMap::new(),
        result_ok(Value::Int(42)),
    );
    assert_native_three_way(
        "#main() -> Result<Int, String>\nResult.Err { error: \"bad\" }",
        HashMap::new(),
        result_err(Value::String("bad".into())),
    );
}

#[test]
fn option_some_tuple_payload_compiles() {
    assert_native_three_way(
        "#main() -> Option<Tuple<Int, String>>\nOption.Some { value: (7, \"x\") }",
        HashMap::new(),
        Value::option_some(Value::Tuple(std::sync::Arc::new(vec![
            Value::Int(7),
            Value::String("x".into()),
        ]))),
    );
}

#[test]
fn tuple_fields_can_hold_option_and_result() {
    assert_native_three_way(
        "#main() -> Tuple<Option<Int>, Result<Int, String>>\n(Option.Some { value: 1 }, Result.Err { error: \"no\" })",
        HashMap::new(),
        Value::Tuple(std::sync::Arc::new(vec![
            Value::option_some(Value::Int(1)),
            result_err(Value::String("no".into())),
        ])),
    );
}

#[test]
fn option_input_identity_compiles() {
    assert_native_three_way(
        "#main(Option<Int> x) -> Option<Int>
x",
        args([("x", Value::option_some(Value::Int(9)))]),
        Value::option_some(Value::Int(9)),
    );
}

#[test]
fn result_input_identity_compiles() {
    assert_native_three_way(
        "#main(Result<Int, String> r) -> Result<Int, String>
r",
        args([("r", result_err(Value::String("no".into())))]),
        result_err(Value::String("no".into())),
    );
}

#[test]
fn option_match_param_compiles() {
    assert_native_three_way(
        "#main(Option<Int> x) -> Int
x match { Some(v): v + 1, None: 0 }",
        args([("x", Value::option_some(Value::Int(41)))]),
        Value::Int(42),
    );
    assert_native_three_way(
        "#main(Option<Int> x) -> Int
x match { Some(v): v + 1, None: 0 }",
        args([("x", Value::option_none())]),
        Value::Int(0),
    );
}

#[test]
fn option_match_direct_payload_access_compiles() {
    assert_native_three_way(
        "#main(Option<Int> x) -> Int
x match { Some: x.value, None: 0 }",
        args([("x", Value::option_some(Value::Int(7)))]),
        Value::Int(7),
    );
}

#[test]
fn result_match_param_compiles() {
    assert_native_three_way(
        "#main(Result<Int, String> r) -> Int
r match { Ok(v): v + 1, Err(e): 0 }",
        args([("r", result_ok(Value::Int(41)))]),
        Value::Int(42),
    );
    assert_native_three_way(
        "#main(Result<Int, String> r) -> Int
r match { Ok(v): v + 1, Err(e): 0 }",
        args([("r", result_err(Value::String("no".into())))]),
        Value::Int(0),
    );
}

#[test]
fn rust_like_option_some_int_return_compiles() {
    assert_native_three_way(
        "#main(Int n) -> Option<Int>\nSome(n)",
        args([("n", Value::Int(7))]),
        Value::option_some(Value::Int(7)),
    );
}

#[test]
fn rust_like_option_none_string_return_compiles() {
    assert_native_three_way(
        "#main() -> Option<String>\nNone",
        HashMap::new(),
        Value::option_none(),
    );
}

#[test]
fn rust_like_result_ok_and_err_return_compile() {
    assert_native_three_way(
        "#main() -> Result<Int, String>\nOk(42)",
        HashMap::new(),
        result_ok(Value::Int(42)),
    );
    assert_native_three_way(
        "#main() -> Result<Int, String>\nErr(\"bad\")",
        HashMap::new(),
        result_err(Value::String("bad".into())),
    );
}

#[test]
fn rust_like_option_some_tuple_payload_compiles() {
    assert_native_three_way(
        "#main() -> Option<Tuple<Int, String>>\nSome((7, \"x\"))",
        HashMap::new(),
        Value::option_some(Value::Tuple(std::sync::Arc::new(vec![
            Value::Int(7),
            Value::String("x".into()),
        ]))),
    );
}

#[test]
fn rust_like_tuple_fields_can_hold_option_and_result() {
    assert_native_three_way(
        "#main() -> Tuple<Option<Int>, Result<Int, String>>\n(Some(1), Err(\"no\"))",
        HashMap::new(),
        Value::Tuple(std::sync::Arc::new(vec![
            Value::option_some(Value::Int(1)),
            result_err(Value::String("no".into())),
        ])),
    );
}

#[test]
fn custom_enum_unit_return_compiles() {
    assert_native_three_way(
        "#enum Stat { Up, Down }\n#main() -> Stat\nStat.Up",
        HashMap::new(),
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
    );
}

#[test]
fn custom_enum_struct_return_compiles() {
    assert_native_three_way(
        "#enum Notification { Email { address: String, subject: String }, Push }\n\
         #main() -> Notification\n\
         Notification.Email { address: \"a@b.c\", subject: \"hi\" }",
        HashMap::new(),
        Value::variant_dict(
            [
                ("address", Value::String("a@b.c".into())),
                ("subject", Value::String("hi".into())),
            ],
            "Email".to_string(),
            "Notification".to_string(),
        ),
    );
}

#[test]
fn custom_enum_tuple_return_compiles() {
    assert_native_three_way(
        "#enum Packet { Pair(Int, String), Empty }\n#main() -> Packet\nPacket.Pair(7, \"x\")",
        HashMap::new(),
        Value::variant_dict(
            [("0", Value::Int(7)), ("1", Value::String("x".into()))],
            "Pair".to_string(),
            "Packet".to_string(),
        ),
    );
}

#[test]
fn custom_enum_unit_ternary_return_compiles() {
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(Bool b) -> Stat
b ? Stat.Up : Stat.Down",
        args([("b", Value::Bool(true))]),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
    );
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(Bool b) -> Stat
b ? Stat.Up : Stat.Down",
        args([("b", Value::Bool(false))]),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Down".to_string(),
            "Stat".to_string(),
        ),
    );
}

#[test]
fn custom_enum_payload_ternary_return_compiles() {
    assert_native_three_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(Bool b) -> Packet
b ? Packet.Pair(7, "x") : Packet.Empty"#,
        args([("b", Value::Bool(true))]),
        Value::variant_dict(
            [("0", Value::Int(7)), ("1", Value::String("x".into()))],
            "Pair".to_string(),
            "Packet".to_string(),
        ),
    );
    assert_native_three_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(Bool b) -> Packet
b ? Packet.Pair(7, "x") : Packet.Empty"#,
        args([("b", Value::Bool(false))]),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Empty".to_string(),
            "Packet".to_string(),
        ),
    );
}

#[test]
fn custom_enum_unit_input_identity_compiles() {
    let input = Value::variant_dict(
        std::iter::empty::<(&str, Value)>(),
        "Down".to_string(),
        "Stat".to_string(),
    );
    assert_native_three_way(
        "#enum Stat { Up, Down }\n#main(Stat s) -> Stat\ns",
        args([("s", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_tuple_input_identity_compiles() {
    let input = Value::variant_dict(
        [("0", Value::Int(7)), ("1", Value::String("x".into()))],
        "Pair".to_string(),
        "Packet".to_string(),
    );
    assert_native_three_way(
        "#enum Packet { Pair(Int, String), Empty }\n#main(Packet p) -> Packet\np",
        args([("p", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_unit_list_input_identity_compiles() {
    let input = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Down".to_string(),
            "Stat".to_string(),
        ),
    ]));
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(List<Stat> xs) -> List<Stat>
xs",
        args([("xs", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_tuple_list_input_identity_compiles() {
    let input = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            [("0", Value::Int(7)), ("1", Value::String("x".into()))],
            "Pair".to_string(),
            "Packet".to_string(),
        ),
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Empty".to_string(),
            "Packet".to_string(),
        ),
    ]));
    assert_native_three_way(
        "#enum Packet { Pair(Int, String), Empty }
#main(List<Packet> xs) -> List<Packet>
xs",
        args([("xs", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_unit_list_literal_return_compiles() {
    let expected = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Down".to_string(),
            "Stat".to_string(),
        ),
    ]));
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main() -> List<Stat>
[Stat.Up, Stat.Down]",
        HashMap::new(),
        expected,
    );
}

#[test]
fn custom_enum_tuple_list_literal_return_compiles() {
    let expected = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            [("0", Value::Int(7)), ("1", Value::String("x".into()))],
            "Pair".to_string(),
            "Packet".to_string(),
        ),
        Value::variant_dict(
            std::iter::empty::<(&str, Value)>(),
            "Empty".to_string(),
            "Packet".to_string(),
        ),
    ]));
    assert_native_three_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main() -> List<Packet>
[Packet.Pair(7, "x"), Packet.Empty]"#,
        HashMap::new(),
        expected,
    );
}

#[test]
fn custom_enum_empty_list_literal_return_compiles() {
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main() -> List<Stat>
[]",
        HashMap::new(),
        Value::List(std::sync::Arc::new(vec![])),
    );
}

#[test]
fn custom_enum_unit_list_map_return_compiles() {
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(List<Int> xs) -> List<Stat>
xs.map((Int x) => x > 0 ? Stat.Up : Stat.Down)",
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Up".to_string(),
                "Stat".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Down".to_string(),
                "Stat".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Up".to_string(),
                "Stat".to_string(),
            ),
        ])),
    );
}

#[test]
fn option_list_map_return_compiles() {
    assert_native_three_way(
        "#main(List<Int> xs) -> List<Option<Int>>
xs.map((Int x) => x > 0 ? Some(x) : None)",
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            Value::option_some(Value::Int(1)),
            Value::option_none(),
            Value::option_some(Value::Int(2)),
        ])),
    );
}

#[test]
fn result_list_map_return_compiles() {
    assert_native_three_way(
        "#main(List<Int> xs) -> List<Result<Int, String>>
xs.map((Int x) => x > 0 ? Ok(x) : Err(\"bad\"))",
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            result_ok(Value::Int(1)),
            result_err(Value::String("bad".into())),
            result_ok(Value::Int(2)),
        ])),
    );
}

#[test]
fn custom_enum_tuple_list_map_return_compiles() {
    assert_native_three_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(List<Int> xs) -> List<Packet>
xs.map((Int x) => x > 0 ? Packet.Pair(x, "x") : Packet.Empty)"#,
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            Value::variant_dict(
                [("0", Value::Int(1)), ("1", Value::String("x".into()))],
                "Pair".to_string(),
                "Packet".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Empty".to_string(),
                "Packet".to_string(),
            ),
            Value::variant_dict(
                [("0", Value::Int(2)), ("1", Value::String("x".into()))],
                "Pair".to_string(),
                "Packet".to_string(),
            ),
        ])),
    );
}

#[test]
fn custom_enum_struct_list_map_return_compiles() {
    assert_native_three_way(
        r#"#enum Msg { Email { code: Int, subject: String }, Push }
#main(List<Int> xs) -> List<Msg>
xs.map((Int x) => x > 0 ? Msg.Email { code: x, subject: "hi" } : Msg.Push)"#,
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            Value::variant_dict(
                [
                    ("code", Value::Int(1)),
                    ("subject", Value::String("hi".into())),
                ],
                "Email".to_string(),
                "Msg".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Push".to_string(),
                "Msg".to_string(),
            ),
            Value::variant_dict(
                [
                    ("code", Value::Int(2)),
                    ("subject", Value::String("hi".into())),
                ],
                "Email".to_string(),
                "Msg".to_string(),
            ),
        ])),
    );
}

#[test]
fn custom_enum_tuple_comprehension_return_compiles() {
    assert_native_three_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(List<Int> xs) -> List<Packet>
[x > 0 ? Packet.Pair(x, "x") : Packet.Empty for x in xs]"#,
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            Value::variant_dict(
                [("0", Value::Int(1)), ("1", Value::String("x".into()))],
                "Pair".to_string(),
                "Packet".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Empty".to_string(),
                "Packet".to_string(),
            ),
            Value::variant_dict(
                [("0", Value::Int(2)), ("1", Value::String("x".into()))],
                "Pair".to_string(),
                "Packet".to_string(),
            ),
        ])),
    );
}

#[test]
fn option_list_filter_return_compiles() {
    let input = Value::List(std::sync::Arc::new(vec![
        Value::option_some(Value::Int(1)),
        Value::option_none(),
        Value::option_some(Value::Int(2)),
    ]));
    assert_native_three_way(
        "#main(List<Option<Int>> xs) -> List<Option<Int>>
xs.filter((Option<Int> x) => true)",
        args([("xs", input.clone())]),
        input,
    );
}

#[test]
fn result_list_filter_return_compiles() {
    let input = Value::List(std::sync::Arc::new(vec![
        result_ok(Value::Int(1)),
        result_err(Value::String("bad".into())),
    ]));
    assert_native_three_way(
        "#main(List<Result<Int, String>> xs) -> List<Result<Int, String>>
xs.filter((Result<Int, String> x) => true)",
        args([("xs", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_unit_list_filter_return_compiles() {
    let input = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Down".to_string(),
            "Stat".to_string(),
        ),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
    ]));
    let expected = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Up".to_string(),
            "Stat".to_string(),
        ),
    ]));
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(List<Stat> xs) -> List<Stat>
xs.filter((Stat s) => s match { Up: true, Down: false })",
        args([("xs", input.clone())]),
        expected.clone(),
    );
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(List<Stat> xs) -> List<Stat>
_list_filter(xs, (Stat s) => s match { Up: true, Down: false })",
        args([("xs", input)]),
        expected,
    );
}

#[test]
fn custom_enum_tuple_list_filter_payload_pattern_compiles() {
    let input = Value::List(std::sync::Arc::new(vec![
        Value::variant_dict(
            [("0", Value::Int(1)), ("1", Value::String("x".into()))],
            "Pair".to_string(),
            "Packet".to_string(),
        ),
        Value::variant_dict(
            Vec::<(&str, Value)>::new(),
            "Empty".to_string(),
            "Packet".to_string(),
        ),
        Value::variant_dict(
            [("0", Value::Int(-1)), ("1", Value::String("n".into()))],
            "Pair".to_string(),
            "Packet".to_string(),
        ),
    ]));
    let expected = Value::List(std::sync::Arc::new(vec![Value::variant_dict(
        [("0", Value::Int(1)), ("1", Value::String("x".into()))],
        "Pair".to_string(),
        "Packet".to_string(),
    )]));
    assert_native_three_way(
        r#"#enum Packet { Pair(Int, String), Empty }
#main(List<Packet> xs) -> List<Packet>
xs.filter((Packet p) => p match { Pair(n, *): n > 0, Empty: false })"#,
        args([("xs", input)]),
        expected,
    );
}

#[test]
fn custom_enum_unit_comprehension_return_compiles() {
    assert_native_three_way(
        "#enum Stat { Up, Down }
#main(List<Int> xs) -> List<Stat>
[x > 0 ? Stat.Up : Stat.Down for x in xs]",
        args([(
            "xs",
            Value::List(std::sync::Arc::new(vec![
                Value::Int(1),
                Value::Int(-1),
                Value::Int(2),
            ])),
        )]),
        Value::List(std::sync::Arc::new(vec![
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Up".to_string(),
                "Stat".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Down".to_string(),
                "Stat".to_string(),
            ),
            Value::variant_dict(
                Vec::<(&str, Value)>::new(),
                "Up".to_string(),
                "Stat".to_string(),
            ),
        ])),
    );
}

#[test]
fn custom_enum_unit_match_param_compiles() {
    let input = Value::variant_dict(
        std::iter::empty::<(&str, Value)>(),
        "Down".to_string(),
        "Stat".to_string(),
    );
    assert_native_three_way(
        "#enum Stat { Up, Down }\n#main(Stat s) -> Int\ns match { Up: 1, Down: 0 }",
        args([("s", input)]),
        Value::Int(0),
    );
}

#[test]
fn custom_enum_struct_match_param_compiles() {
    let input = Value::variant_dict(
        [("address", Value::String("a@b.c".into()))],
        "Email".to_string(),
        "Notification".to_string(),
    );
    assert_native_three_way(
        "#enum Notification { Email { address: String }, Push }\n\
         #main(Notification msg) -> Int\n\
         msg match { Push: 0, Email: 1 }",
        args([("msg", input)]),
        Value::Int(1),
    );
}

#[test]
fn custom_enum_tuple_match_param_with_wildcard_compiles() {
    let input = Value::variant_dict(
        [("0", Value::Int(7)), ("1", Value::String("x".into()))],
        "Pair".to_string(),
        "Packet".to_string(),
    );
    assert_native_three_way(
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Int\n\
         p match { Empty: 0, *: 1 }",
        args([("p", input)]),
        Value::Int(1),
    );
}

#[test]
fn custom_enum_struct_match_payload_field_compiles() {
    let input = Value::variant_dict(
        [("address", Value::String("a@b.c".into()))],
        "Email".to_string(),
        "Notification".to_string(),
    );
    assert_native_three_way(
        "#enum Notification { Email { address: String }, Push }\n\
         #main(Notification msg) -> String\n\
         msg match { Push: \"\", Email: msg.address }",
        args([("msg", input)]),
        Value::String("a@b.c".into()),
    );
}

#[test]
fn custom_enum_tuple_match_payload_index_compiles() {
    let input = Value::variant_dict(
        [("0", Value::Int(7)), ("1", Value::String("x".into()))],
        "Pair".to_string(),
        "Packet".to_string(),
    );
    assert_native_three_way(
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Int\n\
         p match { Empty: 0, Pair: p.0 }",
        args([("p", input)]),
        Value::Int(7),
    );
}

#[test]
fn custom_enum_tuple_match_payload_pattern_compiles() {
    let input = Value::variant_dict(
        [("0", Value::Int(7)), ("1", Value::String("x".into()))],
        "Pair".to_string(),
        "Packet".to_string(),
    );
    assert_native_three_way(
        "#enum Packet { Pair(Int, String), Empty }\n\
         #main(Packet p) -> Int\n\
         p match { Pair(n, *): n + 1, Empty: 0 }",
        args([("p", input)]),
        Value::Int(8),
    );
}

#[test]
fn custom_enum_struct_match_payload_pattern_compiles() {
    let input = Value::variant_dict(
        [
            ("code", Value::Int(41)),
            ("subject", Value::String("hi".into())),
        ],
        "Email".to_string(),
        "Notification".to_string(),
    );
    assert_native_three_way(
        "#enum Notification { Email { code: Int, subject: String }, Push }\n\
         #main(Notification msg) -> Int\n\
         msg match { Notification.Email { code, subject: * }: code + 1, Push: 0 }",
        args([("msg", input)]),
        Value::Int(42),
    );
}

#[test]
fn custom_generic_enum_input_identity_compiles() {
    let input = Value::variant_dict(
        [("0", Value::Int(7))],
        "Some".to_string(),
        "Box".to_string(),
    );
    assert_native_three_way(
        "#enum Box<T> { Some(T), None }\n#main(Box<Int> b) -> Box<Int>\nb",
        args([("b", input.clone())]),
        input,
    );
}

#[test]
fn custom_generic_enum_match_payload_pattern_compiles() {
    let input = Value::variant_dict(
        [("0", Value::Int(7))],
        "Some".to_string(),
        "Box".to_string(),
    );
    assert_native_three_way(
        "#enum Box<T> { Some(T), None }\n\
         #main(Box<Int> b) -> Int\n\
         b match { Some(n): n + 1, None: 0 }",
        args([("b", input)]),
        Value::Int(8),
    );
}

#[test]
fn custom_enum_nested_tuple_payload_identity_compiles() {
    let input = Value::variant_dict(
        [(
            "0",
            Value::Tuple(std::sync::Arc::new(vec![
                Value::Int(7),
                Value::String("x".into()),
            ])),
        )],
        "Nested".to_string(),
        "Payload".to_string(),
    );
    assert_native_three_way(
        "#enum Payload { Nested(Tuple<Int, String>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args([("p", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_list_payload_identity_compiles() {
    let input = Value::variant_dict(
        [(
            "0",
            Value::List(std::sync::Arc::new(vec![Value::Int(1), Value::Int(2)])),
        )],
        "Numbers".to_string(),
        "Payload".to_string(),
    );
    assert_native_three_way(
        "#enum Payload { Numbers(List<Int>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args([("p", input.clone())]),
        input,
    );
}

#[test]
fn custom_enum_option_result_payload_identity_compiles() {
    let maybe = Value::variant_dict(
        [("0", Value::option_some(Value::Int(9)))],
        "Maybe".to_string(),
        "Payload".to_string(),
    );
    assert_native_three_way(
        "#enum Payload { Maybe(Option<Int>), Outcome(Result<Int, String>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args([("p", maybe.clone())]),
        maybe,
    );

    let outcome = Value::variant_dict(
        [("0", result_err(Value::String("no".into())))],
        "Outcome".to_string(),
        "Payload".to_string(),
    );
    assert_native_three_way(
        "#enum Payload { Maybe(Option<Int>), Outcome(Result<Int, String>), Empty }\n\
         #main(Payload p) -> Payload\n\
         p",
        args([("p", outcome.clone())]),
        outcome,
    );
}

#[test]
fn custom_generic_enum_return_constructor_compiles() {
    let expected = Value::variant_dict(
        [("0", Value::Int(7))],
        "Some".to_string(),
        "Box".to_string(),
    );
    assert_native_three_way(
        "#enum Box<T> { Some(T), None }\n#main() -> Box<Int>\nBox.Some(7)",
        HashMap::new(),
        expected,
    );
}

#[test]
fn custom_generic_enum_struct_payload_constructor_compiles() {
    let expected = Value::variant_dict(
        [("value", Value::Int(7))],
        "Some".to_string(),
        "Box".to_string(),
    );
    assert_native_three_way(
        "#enum Box<T> { Some { value: T }, None }
#main() -> Box<Int>
Box.Some { value: 7 }",
        HashMap::new(),
        expected,
    );
}

#[test]
fn custom_enum_nested_payload_constructors_compile() {
    assert_native_three_way(
        "#enum Payload { Nested(Tuple<Int, String>), Numbers(List<Int>), Maybe(Option<Int>), Empty }\n\
         #main() -> Payload\n\
         Payload.Nested((7, \"x\"))",
        HashMap::new(),
        Value::variant_dict(
            [(
                "0",
                Value::Tuple(std::sync::Arc::new(vec![
                    Value::Int(7),
                    Value::String("x".into()),
                ])),
            )],
            "Nested".to_string(),
            "Payload".to_string(),
        ),
    );
    assert_native_three_way(
        "#enum Payload { Nested(Tuple<Int, String>), Numbers(List<Int>), Maybe(Option<Int>), Empty }\n\
         #main() -> Payload\n\
         Payload.Numbers([1, 2])",
        HashMap::new(),
        Value::variant_dict(
            [("0", Value::List(std::sync::Arc::new(vec![Value::Int(1), Value::Int(2)])))],
            "Numbers".to_string(),
            "Payload".to_string(),
        ),
    );
    assert_native_three_way(
        "#enum Payload { Nested(Tuple<Int, String>), Numbers(List<Int>), Maybe(Option<Int>), Empty }\n\
         #main() -> Payload\n\
         Payload.Maybe(Some(9))",
        HashMap::new(),
        Value::variant_dict(
            [("0", Value::option_some(Value::Int(9)))],
            "Maybe".to_string(),
            "Payload".to_string(),
        ),
    );
}
