//! Test module extracted from lib.rs to keep the public root minimal.
//! Wired in via `#[cfg(test)] mod eval_tests;` in lib.rs.
//!
//! Lives inside the crate (rather than `tests/`) because some tests
//! need access to `pub(crate)` items such as `Context::module_resolvers`.

use super::*;
use relon_parser::expr::parse_expr;
use relon_parser::parse_document;
use relon_parser::Expr;
use relon_parser::Span;

fn parse_doc(source: &str) -> relon_parser::Node {
    parse_document(source).expect("Parser failed")
}

fn eval_doc(source: &str) -> Result<Value, RuntimeError> {
    let node = parse_doc(source);
    let ctx = Context::new().with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&std::sync::Arc::new(Scope::default()))
}

/// Test scaffolding: build a Context with full grants + trusted FS
/// resolver. Equivalent to what the CLI / facade do when running
/// host-owned files. Production code should spell the grants out
/// inline so the trust scope is visible at the call site.
fn fully_granted_ctx() -> Context {
    let mut ctx = Context::sandboxed();
    ctx.capabilities = Capabilities::all_granted();
    ctx.prepend_module_resolver(std::sync::Arc::new(
        crate::module::FilesystemModuleResolver::trusted(),
    ));
    ctx
}

fn assert_number_type_mismatch(source: &str, found_type: &str) {
    let result = eval_doc(source);
    assert!(matches!(
        result,
        Err(RuntimeError::TypeMismatch { expected, found, .. })
            if expected == "Number" && found == found_type
    ));
}

fn assert_numeric_overflow(source: &str) {
    let result = eval_doc(source);
    assert!(
        matches!(result, Err(RuntimeError::NumericOverflow(_))),
        "expected NumericOverflow, got {result:?}"
    );
}

#[test]
fn test_user_defined_meta_logic() {
    let node = parse_doc(
        r#"{
        shout(v): v + "!!!",
        multiply(a, b): a * b,
        "result_fn": multiply(10, 5),
        "result_dec": @shout "hello"
    }"#,
    );
    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope::default());

    let result = eval.eval(&node, &scope).expect("Evaluation failed");

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("result_fn").unwrap(), &Value::Int(50));
        assert_eq!(
            map.map.get("result_dec").unwrap(),
            &Value::String("hello!!!".to_string())
        );
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_invalid_fn_name() {
    let node = parse_doc(
        r#"{
        "invalid name": (x) => x + 1
    }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope::default());

    let result = eval.eval(&node, &scope);
    assert!(result.is_err());
    if let Err(RuntimeError::InvalidIdentifier(name, _)) = result {
        assert_eq!(name, "invalid name");
    } else {
        panic!("Expected InvalidIdentifier error");
    }
}

#[test]
fn test_pipe_operator() {
    let mut input = Span::new(r#"[1, 2, 3] | len()"#);
    let node = parse_expr(&mut input).unwrap();
    let ctx = Context::new();
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()))
        .unwrap();
    assert_eq!(result, Value::Int(3));
}

#[test]
fn test_mixed_numeric_operations() {
    let result = eval_doc(
        r#"{
        "add": 1 + 2.5,
        "sub": 5 - 2.5,
        "mul": 2 * 1.5,
        "div": 5 / 2.0,
        "mod": 5 % 2.0,
        "lt": 1 < 2.5,
        "ge": 2.0 >= 2
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("add").unwrap(),
            &Value::Float(ordered_float::OrderedFloat(3.5))
        );
        assert_eq!(
            map.map.get("sub").unwrap(),
            &Value::Float(ordered_float::OrderedFloat(2.5))
        );
        assert_eq!(
            map.map.get("mul").unwrap(),
            &Value::Float(ordered_float::OrderedFloat(3.0))
        );
        assert_eq!(
            map.map.get("div").unwrap(),
            &Value::Float(ordered_float::OrderedFloat(2.5))
        );
        assert_eq!(
            map.map.get("mod").unwrap(),
            &Value::Float(ordered_float::OrderedFloat(1.0))
        );
        assert_eq!(map.map.get("lt").unwrap(), &Value::Bool(true));
        assert_eq!(map.map.get("ge").unwrap(), &Value::Bool(true));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_invalid_numeric_operands_are_rejected() {
    assert_number_type_mismatch(r#"{ "value": "x" / 2 }"#, "String");
    assert_number_type_mismatch(r#"{ "value": 2 / "x" }"#, "String");
    assert_number_type_mismatch(r#"{ "value": null % 2 }"#, "Null");
    assert_number_type_mismatch(r#"{ "value": "x" < 2 }"#, "String");
    assert_number_type_mismatch(r#"{ "value": -false }"#, "Bool");
}

#[test]
fn test_integer_overflow_is_deterministic_error() {
    assert_numeric_overflow(r#"{ "value": 9223372036854775807 + 1 }"#);
    assert_numeric_overflow(r#"{ "value": -9223372036854775807 - 2 }"#);
    assert_numeric_overflow(r#"{ "value": 3037000500 * 3037000500 }"#);
    assert_numeric_overflow(r#"{ "value": (-9223372036854775807 - 1) / -1 }"#);
    assert_numeric_overflow(r#"{ "value": (-9223372036854775807 - 1) % -1 }"#);
    assert_numeric_overflow(r#"{ "value": -(-9223372036854775807 - 1) }"#);
}

#[test]
fn test_range_rejects_non_integer_arguments() {
    let result = eval_doc(r#"{ "value": range("3") }"#);
    assert!(matches!(
        result,
        Err(RuntimeError::TypeMismatch { expected, found, .. })
            if expected == "Int" && found == "String"
    ));
}

#[test]
fn test_readable_constraint_decorators() {
    let result = eval_doc(
        r#"{
        @ensure.int
        @ensure.at_least(1)
        "port": 3,
        @ensure.required_fields(["port"])
        "config": { "port": 3 }
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("port").unwrap(), &Value::Int(3));
        assert!(matches!(map.map.get("config").unwrap(), Value::Dict(_)));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_required_fields_checks_presence_not_truthiness() {
    let result = eval_doc(
        r#"{
        @ensure.required_fields(["enabled", "retries", "name"])
        "config": {
            "enabled": false,
            "retries": 0,
            "name": ""
        }
    }"#,
    )
    .unwrap();

    assert!(matches!(result, Value::Dict(_)));
}

#[test]
fn test_string_stdlib() {
    let result = eval_doc(
        r#"#import string from "std/string"
        {
        "words": string.split("rust,config,dsl", ","),
        "joined": string.join(&sibling.words, "-"),
        "replaced": string.replace("hello world", "world", "relon"),
        "upper": string.upper("Relon"),
        "lower": string.lower("Relon"),
        "has_config": string.contains("rust config dsl", "config")
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("joined").unwrap(),
            &Value::String("rust-config-dsl".to_string())
        );
        assert_eq!(
            map.map.get("replaced").unwrap(),
            &Value::String("hello relon".to_string())
        );
        assert_eq!(
            map.map.get("upper").unwrap(),
            &Value::String("RELON".to_string())
        );
        assert_eq!(
            map.map.get("lower").unwrap(),
            &Value::String("relon".to_string())
        );
        assert_eq!(map.map.get("has_config").unwrap(), &Value::Bool(true));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_dict_stdlib() {
    let result = eval_doc(
        r#"#import dict from "std/dict"
        #import list from "std/list"
        {
        "base": { "a": 1, "b": 2 },
        "override": { "b": 3, "c": 4 },
        "merged": dict.merge(&sibling.base, &sibling.override),
        "keys": dict.keys(&sibling.merged),
        "values": dict.values(&sibling.merged),
        "has_b": dict.has_key(&sibling.merged, "b"),
        "has_z": dict.has_key(&sibling.merged, "z"),
        "list_has_b": list.contains(&sibling.keys, "b")
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("keys").unwrap(),
            &Value::list(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ])
        );
        assert_eq!(
            map.map.get("values").unwrap(),
            &Value::list(vec![Value::Int(1), Value::Int(3), Value::Int(4)])
        );
        assert_eq!(map.map.get("has_b").unwrap(), &Value::Bool(true));
        assert_eq!(map.map.get("has_z").unwrap(), &Value::Bool(false));
        assert_eq!(map.map.get("list_has_b").unwrap(), &Value::Bool(true));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_virtual_stdlib_modules() {
    let result = eval_doc(
        r#"#import list from "std/list"
        #import math from "std/math"
        #import value from "std/value"
        #import is from "std/is"
        #import string from "std/string"
        #import dict from "std/dict"
        {
            "first": list.first([10, 20, 30]),
            "compact": list.compact([1, null, 2]),
            "clamped": math.clamp(12, 0, 10),
            "defaulted": value.default(null, "fallback"),
            "kept_false": value.default(false, true),
            "is_number": is.number(1.5),
            "is_empty": is.empty([]),
            "joined": string.join(["a", "b"], "-"),
            "has_key": dict.has_key({ "port": 80 }, "port")
        }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("first").unwrap(), &Value::Int(10));
        assert_eq!(
            map.map.get("compact").unwrap(),
            &Value::list(vec![Value::Int(1), Value::Int(2)])
        );
        assert_eq!(map.map.get("clamped").unwrap(), &Value::Int(10));
        assert_eq!(
            map.map.get("defaulted").unwrap(),
            &Value::String("fallback".to_string())
        );
        assert_eq!(map.map.get("kept_false").unwrap(), &Value::Bool(false));
        assert_eq!(map.map.get("is_number").unwrap(), &Value::Bool(true));
        assert_eq!(map.map.get("is_empty").unwrap(), &Value::Bool(true));
        assert_eq!(
            map.map.get("joined").unwrap(),
            &Value::String("a-b".to_string())
        );
        assert_eq!(map.map.get("has_key").unwrap(), &Value::Bool(true));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_validation_custom_messages() {
    let node = parse_doc(
        r#"{
        @ensure.at_least(1024, "port must be >= 1024")
        "port": 80
    }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()));

    assert!(matches!(
        result,
        Err(RuntimeError::ValidationError(message, _)) if message == "port must be >= 1024"
    ));
}

#[test]
fn test_cross_field_validation() {
    let node = parse_doc(
        r#"@ensure.required_fields(["host", "port"])
        @ensure.requires("tls", "cert")
        @ensure.fields_equal("password", "confirm")
        {
            "host": "localhost",
            "port": 8080,
            "tls": true,
            "cert": "cert.pem",
            "password": "secret",
            "confirm": "secret"
        }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()))
        .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("host").unwrap(),
            &Value::String("localhost".to_string())
        );
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_cross_field_validation_custom_message() {
    let node = parse_doc(
        r#"@ensure.requires("tls", "cert", "cert is required when tls is enabled")
        {
            "tls": true
        }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()));

    assert!(matches!(
        result,
        Err(RuntimeError::ValidationError(message, _))
            if message == "cert is required when tls is enabled"
    ));
}

#[test]
fn test_spread_operator() {
    let node = parse_doc(
        r#"{ 
        "base": { "x": 1 },
        "full": { ...&sibling.base, "y": 2 }
    }"#,
    );
    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()))
        .unwrap();
    if let Value::Dict(map) = result {
        let full = map.map.get("full").unwrap();
        if let Value::Dict(inner) = full {
            assert_eq!(inner.map.get("x").unwrap(), &Value::Int(1));
            assert_eq!(inner.map.get("y").unwrap(), &Value::Int(2));
        } else {
            panic!()
        }
    } else {
        panic!()
    }
}

#[test]
fn test_reference_resolution_sees_spread_keys() {
    let node = parse_doc(
        r#"{
        "base": { "port": 8080 },
        "app": { ...&sibling.base, "host": "localhost" },
        "port_copy": &sibling.app.port,
        "late_spread": { "port": 3000, ...&sibling.base },
        "late_spread_copy": &sibling.late_spread.port,
        "late_key": { ...&sibling.base, "port": 3000 },
        "late_key_copy": &sibling.late_key.port
    }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()))
        .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("port_copy").unwrap(), &Value::Int(8080));
        assert_eq!(map.map.get("late_spread_copy").unwrap(), &Value::Int(8080));
        assert_eq!(map.map.get("late_key_copy").unwrap(), &Value::Int(3000));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_duplicate_keys_and_references_use_last_value() {
    let result = eval_doc(
        r#"{
        "a": 1,
        "a": 2,
        "b": &sibling.a
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("a").unwrap(), &Value::Int(2));
        assert_eq!(map.map.get("b").unwrap(), &Value::Int(2));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_closure_body_references_use_closure_body_root() {
    let result = eval_doc(
        r#"{
        make(x): {
            "b": &sibling.a,
            "a": x
        },
        "one": make(1)
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        let Value::Dict(one) = map.map.get("one").unwrap() else {
            panic!("Expected Dict");
        };
        assert_eq!(one.map.get("a").unwrap(), &Value::Int(1));
        assert_eq!(one.map.get("b").unwrap(), &Value::Int(1));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_reference_resolution_caches_resolved_paths() {
    let node = parse_doc(
        r#"{
        "a": 10 + 5,
        "b": &sibling.a,
        "c": &sibling.a
    }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()))
        .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("b").unwrap(), &Value::Int(15));
        assert_eq!(map.map.get("c").unwrap(), &Value::Int(15));
    } else {
        panic!("Expected Dict");
    }

    assert!(ctx
        .path_cache
        .lock()
        .unwrap()
        .values()
        .any(|value| value == &Value::Int(15)));
}

#[test]
fn test_circular_reference_with_thunks() {
    let node = parse_doc(
        r#"{
        "a": &sibling.b,
        "b": &sibling.a
    }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()));

    assert!(matches!(
        result,
        Err(RuntimeError::CircularReference { .. })
    ));
}

#[test]
fn test_list_comprehension() {
    let mut input = Span::new(r#"[x * 2 for x in range(5) if x % 2 == 0]"#);
    let node = parse_expr(&mut input).unwrap();
    let ctx = Context::new();
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval(&node, &std::sync::Arc::new(Scope::default()))
        .unwrap();
    assert_eq!(
        result,
        Value::list(vec![Value::Int(0), Value::Int(4), Value::Int(8)])
    );
}

#[test]
fn test_reference_resolution() {
    let node = parse_doc(
        r#"{
        "a": 10,
        "b": &sibling.a + 5,
        "c": {
            "d": &uncle.b * 2
        }
    }"#,
    );

    let ctx = Context::new().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope::default());

    let result = eval.eval(&node, &scope).expect("Evaluation failed");

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("a").unwrap(), &Value::Int(10));
        assert_eq!(map.map.get("b").unwrap(), &Value::Int(15)); // 10 + 5

        if let Value::Dict(inner) = map.map.get("c").unwrap() {
            assert_eq!(inner.map.get("d").unwrap(), &Value::Int(30)); // 15 * 2
        } else {
            panic!()
        }
    } else {
        panic!()
    }
}

#[test]
fn test_circular_import() {
    let node = parse_doc(r#"#import a from "tests_assets/a.relon" {}"#);
    let ctx = fully_granted_ctx().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope {
        current_dir: env!("CARGO_MANIFEST_DIR").to_string(),
        ..Default::default()
    });

    let result = eval.eval(&node, &scope);
    assert!(result.is_err());
    if let Err(RuntimeError::CircularImport(chain, _)) = result {
        assert!(chain.len() >= 2);
    } else {
        panic!("Expected CircularImport error, got: {:?}", result);
    }
}

#[test]
fn test_import_cache_uses_canonical_paths() {
    let dir = std::env::temp_dir().join(format!("relon-canonical-import-{}", std::process::id()));
    let subdir = dir.join("sub");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(dir.join("lib.relon"), r#"{ value: 1 }"#).unwrap();

    let node = parse_doc(
        r#"#import a from "sub/../lib.relon"
        #import b from "lib.relon"
        {}"#,
    );
    let ctx = fully_granted_ctx().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });

    eval.eval(&node, &scope).unwrap();

    assert_eq!(ctx.module_cache.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_imported_module_references_use_module_root() {
    let dir = std::env::temp_dir().join(format!(
        "relon-module-reference-root-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.relon"),
        r#"{
            "a": 1,
            "b": &root.a
        }"#,
    )
    .unwrap();

    let node = parse_doc(
        r#"#import lib from "lib.relon"
        {
            "b": lib.b
        }"#,
    );
    let ctx = fully_granted_ctx().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });

    let result = eval.eval(&node, &scope).unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("b").unwrap(), &Value::Int(1));
    } else {
        panic!("Expected Dict");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_loading_modules_restored_after_module_parse_error() {
    let dir = std::env::temp_dir().join(format!("relon-import-parse-error-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bad.relon"), "{} trailing").unwrap();

    let node = parse_doc(r#"#import bad from "bad.relon" {}"#);
    let ctx = fully_granted_ctx().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });

    let result = eval.eval(&node, &scope);

    assert!(matches!(result, Err(RuntimeError::ModuleParseError { .. })));
    assert!(ctx.loading_modules.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_where_expression() {
    let result = eval_doc(
        r#"{
        "calc": (a + b * c) where { a: 10, b: 2, c: 5 }
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(map.map.get("calc").unwrap(), &Value::Int(20));
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_runtime_type_checker_success() {
    let result = eval_doc(
        r#"{
        String name: "Relon",
        Int age: 25,
        List<Int> numbers: [1, 2, 3],
        Dict<String, Bool> flags: { "active": true, "hidden": false }
    }"#,
    )
    .unwrap();
    assert!(matches!(result, Value::Dict(_)));
}

#[test]
fn test_runtime_type_checker_failure_primitive() {
    let result = eval_doc(r#"{ Int age: "25" }"#);
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_runtime_type_checker_failure_generic_list() {
    let result = eval_doc(r#"{ List<Int> numbers: [1, "2", 3] }"#);
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_runtime_type_checker_failure_generic_dict() {
    let result = eval_doc(r#"{ Dict<String, Int> scores: { "a": 100, "b": "90" } }"#);
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_custom_schema_validation() {
    let result = eval_doc(
        r#"{
        #schema
        UserSchema: {
            String name: *,
            Int age: *
        },

        UserSchema admin: {
            name: "Alice",
            age: 30
        }
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        let admin = map.map.get("admin").unwrap();
        if let Value::Dict(admin_map) = admin {
            assert_eq!(
                admin_map.map.get("name").unwrap(),
                &Value::String("Alice".to_string())
            );
            assert_eq!(admin_map.map.get("age").unwrap(), &Value::Int(30));
        } else {
            panic!("Expected Dict for admin");
        }
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_custom_schema_validation_failure() {
    let result = eval_doc(
        r#"{
        #schema
        UserSchema: {
            String name: *,
            Int age: *
        },

        UserSchema admin: {
            name: "Alice",
            age: "30"
        }
    }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_schema_predicates() {
    let result = eval_doc(
        r#"{
        #schema
        Server: {
            Int port: (p) => p > 0 && p < 65536,
            String status: Enum<"up", "down">
        },

        Server s1: { port: 8080, status: "up" }
    }"#,
    )
    .unwrap();
    assert!(matches!(result, Value::Dict(_)));
}

#[test]
fn test_schema_predicate_failure() {
    let result = eval_doc(
        r#"{
        #schema
        Server: {
            Int port: (p) => p > 0 && p < 65536
        },

        Server s: { port: 70000 }
    }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_optional_chaining() {
    let result = eval_doc(
        r#"{
        data: { profile: { email: "alice@example.com" } },
        email: &sibling.data.profile?.email,
        missing: &sibling.data?.other?.field,
        default: &sibling.data?.other?.field || "default"
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("email").unwrap(),
            &Value::String("alice@example.com".to_string())
        );
        assert_eq!(map.map.get("missing").unwrap(), &Value::Null);
        assert_eq!(
            map.map.get("default").unwrap(),
            &Value::String("default".to_string())
        );
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_enum_type() {
    let result = eval_doc(
        r#"{
        #schema
        Theme: {
            mode: Enum<"light", "dark", "system">,
            id: Enum<Int, String>
        },

        Theme t1: { mode: "light", id: 123 },
        Theme t2: { mode: "dark", id: "abc" }
    }"#,
    )
    .unwrap();
    assert!(matches!(result, Value::Dict(_)));
}

#[test]
fn test_enum_type_failure() {
    let result = eval_doc(
        r#"{
        #schema
        Theme: { mode: Enum<"light", "dark"> },
        Theme t: { mode: "other" }
    }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_identity_guard_revalidation() {
    let result = eval_doc(
        r#"{
        #schema User: { 
            String name: *,
            #expect "Age must be positive"
            Int age: (a) => a > 0
        },

        User alice: { name: "Alice", age: 25 },

        // This should fail because it violates User's age constraint
        invalid_alice: &sibling.alice + { age: -1 }
    }"#,
    );
    match result {
        Err(RuntimeError::TypeMismatch { expected, .. }) => {
            assert_eq!(expected, "Age must be positive");
        }
        _ => panic!("Expected TypeMismatch error from Identity Guard"),
    }
}

#[test]
fn test_schema_composition_mixins() {
    let result = eval_doc(
        r#"{
        #schema Base: { String type: * },
        #schema Button: &sibling.Base + { String label: * },

        Button ok_btn: { type: "btn", label: "OK" },

        // This should fail because 'type' is missing (Base requirement)
        invalid_btn: { label: "Cancel" } match {
            Button: "VALID",
            *: "INVALID"
        }
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("invalid_btn").unwrap(),
            &Value::String("INVALID".to_string())
        );
    } else {
        panic!();
    }
}

#[test]
fn test_schema_composition_defaults() {
    let result = eval_doc(
        r#"{
        #schema Base: { #default "info" String level: * },
        #schema Error: &sibling.Base + { level: "error" },

        Error e: {}
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        let e = map.map.get("e").unwrap();
        if let Value::Dict(ed) = e {
            assert_eq!(
                ed.map.get("level").unwrap(),
                &Value::String("error".to_string())
            );
        } else {
            panic!();
        }
    } else {
        panic!();
    }
}

#[test]
fn test_schema_plus_dict_adds_typed_fields() {
    // `Schema + Dict_AST` should treat typed entries on the RHS as new
    // schema fields, not just defaults — so missing required fields fail
    // validation.
    let ok = eval_doc(
        r#"{
        #schema Base: { String name: * },
        #schema User: &sibling.Base + { Int age: * },

        User alice: { name: "Alice", age: 30 }
    }"#,
    )
    .unwrap();
    assert!(matches!(ok, Value::Dict(_)));

    let missing = eval_doc(
        r#"{
        #schema Base: { String name: * },
        #schema User: &sibling.Base + { Int age: * },

        User alice: { name: "Alice" }
    }"#,
    );
    assert!(matches!(missing, Err(RuntimeError::TypeMismatch { .. })));

    let wrong_type = eval_doc(
        r#"{
        #schema Base: { String name: * },
        #schema User: &sibling.Base + { Int age: * },

        User alice: { name: "Alice", age: "thirty" }
    }"#,
    );
    assert!(matches!(wrong_type, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_deep_merge() {
    let result = eval_doc(
        r#"#import dict from "std/dict"
        {
        base: { style: { color: "blue", size: 14 }, active: false },

        // Using + operator
        button1: &sibling.base + { style: { color: "red" }, active: true },

        // Using dict.merge
        button2: dict.merge(&sibling.base, { style: { size: 20 } })
    }"#,
    )
    .unwrap();

    if let Value::Dict(map) = result {
        if let Value::Dict(btn1) = map.map.get("button1").unwrap() {
            assert_eq!(btn1.map.get("active").unwrap(), &Value::Bool(true));
            if let Value::Dict(style) = btn1.map.get("style").unwrap() {
                assert_eq!(
                    style.map.get("color").unwrap(),
                    &Value::String("red".to_string())
                );
                assert_eq!(style.map.get("size").unwrap(), &Value::Int(14));
            // padding preserved
            } else {
                panic!("Expected style dict");
            }
        } else {
            panic!("Expected Dict for btn1");
        }

        if let Value::Dict(btn2) = map.map.get("button2").unwrap() {
            assert_eq!(btn2.map.get("active").unwrap(), &Value::Bool(false));
            if let Value::Dict(style) = btn2.map.get("style").unwrap() {
                assert_eq!(
                    style.map.get("color").unwrap(),
                    &Value::String("blue".to_string())
                );
                assert_eq!(style.map.get("size").unwrap(), &Value::Int(20));
            } else {
                panic!("Expected style dict");
            }
        } else {
            panic!("Expected Dict for btn2");
        }
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_schema_composition_and_combines_predicates() {
    // Base requires `port > 0`; Derived adds `port < 100`.
    // Both constraints must hold after composition.
    let ok = eval_doc(
        r#"{
        #schema Base: { Int port: (p) => p > 0 },
        #schema Derived: &sibling.Base + { Int port: (p) => p < 100 },

        Derived in_range: { port: 50 }
    }"#,
    )
    .unwrap();
    assert!(matches!(ok, Value::Dict(_)));

    // Violates the Derived constraint (port < 100): must fail because
    // composition is now AND, not "right side wins".
    let too_high = eval_doc(
        r#"{
        #schema Base: { Int port: (p) => p > 0 },
        #schema Derived: &sibling.Base + { Int port: (p) => p < 100 },

        Derived too_high: { port: 200 }
    }"#,
    );
    assert!(matches!(too_high, Err(RuntimeError::TypeMismatch { .. })));

    // Violates the Base constraint (port > 0): must still fail under
    // composition — the right-hand `< 100` predicate doesn't shadow it.
    let negative = eval_doc(
        r#"{
        #schema Base: { Int port: (p) => p > 0 },
        #schema Derived: &sibling.Base + { Int port: (p) => p < 100 },

        Derived negative: { port: -5 }
    }"#,
    );
    assert!(matches!(negative, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn recursion_limit_uses_dedicated_error_kind() {
    // A self-referential typed binding that nests beyond the
    // type-check recursion bound should fail with the dedicated
    // `RecursionLimitExceeded` variant — not borrow `StepLimitExceeded`,
    // whose `limit` field semantically counts evaluator steps, not
    // recursion depth.
    //
    // We run on a generous-stack thread because debug-build dict
    // evaluation eats stack frames quickly: the platform default
    // (~512 KB on macOS test threads) can blow before the typecheck
    // bound trips. 8 MB gives the bound room to fire instead.
    let handle = std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let mut deeply_nested = String::new();
            for _ in 0..150 {
                deeply_nested.push_str("{ next: ");
            }
            deeply_nested.push_str("null");
            for _ in 0..150 {
                deeply_nested.push_str(" }");
            }
            let source = format!(
                r#"{{
                    #schema
                    Cell: {{
                        Cell? next: *
                    }},
                    Cell root: {deeply_nested}
                }}"#
            );
            eval_doc(&source)
        })
        .unwrap();
    let result = handle.join().unwrap();
    assert!(
        matches!(&result, Err(RuntimeError::RecursionLimitExceeded { .. })),
        "expected RecursionLimitExceeded, got {result:?}"
    );
}

#[test]
fn test_recursive_schema() {
    let result = eval_doc(
        r#"{
        #schema
        Menu: {
            String title: *,
            List<Menu>? children: *
        },

        Menu root: {
            title: "Home",
            children: [
                { title: "Products", children: [] },
                { title: "About" }
            ]
        }
    }"#,
    )
    .unwrap();
    assert!(matches!(result, Value::Dict(_)));
}

#[test]
fn test_custom_error_message() {
    let result = eval_doc(
        r#"{
        #schema
        Server: {
            #expect "Port must be between 0 and 65535"
            Int port: (p) => p > 0 && p < 65536
        },

        Server s: { port: 70000 }
    }"#,
    );
    match result {
        Err(RuntimeError::TypeMismatch { expected, .. }) => {
            assert_eq!(expected, "Port must be between 0 and 65535");
        }
        _ => panic!("Expected TypeMismatch with custom message"),
    }
}

#[test]
fn test_match_expression() {
    let result = eval_doc(
        r#"{
        #schema Image: { name: String, url: String },
        #schema Text: { name: String, content: String },
        
        data: { name: "img", url: "http://example.com/a.png" },
        
        render: &sibling.data match {
            Image: "IMAGE",
            Text: "TEXT",
            *: "UNKNOWN"
        }
    }"#,
    )
    .unwrap();
    if let Value::Dict(map) = result {
        assert_eq!(
            map.map.get("render").unwrap(),
            &Value::String("IMAGE".to_string())
        );
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_list_relative_references() {
    let result = eval_doc(
        r#"[
        { title: "Step 1", active: true },
        { title: "Step 2", enabled: &prev.active },
        { index: &index, has_prev: &prev != null }
    ]"#,
    )
    .unwrap();
    if let Value::List(l) = result {
        if let Value::Dict(ref m1) = l[1] {
            assert_eq!(m1.map.get("enabled").unwrap(), &Value::Bool(true));
        } else {
            panic!("Expected Dict at index 1");
        }
        if let Value::Dict(ref m2) = l[2] {
            assert_eq!(m2.map.get("index").unwrap(), &Value::Int(2));
            assert_eq!(m2.map.get("has_prev").unwrap(), &Value::Bool(true));
        } else {
            panic!("Expected Dict at index 2");
        }
    } else {
        panic!("Expected List");
    }
}

#[test]
fn test_nominal_branding() {
    let result = eval_doc(
        r#"{
        #schema Button: { String label: * },
        #schema Link: { String label: * },
        
        Button b: { label: "Ok" },
        Link l:   { label: "Home" },
        
        check_b: &sibling.b match {
            Button: "is_button",
            Link: "is_link"
        },
        check_l: &sibling.l match {
            Button: "is_button",
            Link: "is_link"
        }
    }"#,
    )
    .unwrap();

    if let Value::Dict(d) = result {
        assert_eq!(
            d.map.get("check_b").unwrap(),
            &Value::String("is_button".to_string())
        );
        assert_eq!(
            d.map.get("check_l").unwrap(),
            &Value::String("is_link".to_string())
        );
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_brand_decorator_validates_and_brands_dict() {
    // `#brand Weather` at field-value position is the decorator-form
    // analogue of `Weather w: {...}`: it runs `check_type` against the
    // schema and writes `dict.brand`. Verify both effects on a value
    // that satisfies the schema.
    let result = eval_doc(
        r#"{
        #schema Weather: { String location: *, Int temperature: * },

        w: #brand Weather {
            location: "Shanghai",
            temperature: 22
        },

        kind: &sibling.w match {
            Weather: "is_weather",
            *: "other"
        }
    }"#,
    )
    .unwrap();

    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(w) = d.map.get("w").unwrap() else {
        panic!("Expected Dict for w");
    };
    assert_eq!(w.brand.as_deref(), Some("Weather"));
    assert_eq!(
        w.map.get("location").unwrap(),
        &Value::String("Shanghai".to_string())
    );
    assert_eq!(w.map.get("temperature").unwrap(), &Value::Int(22));
    assert_eq!(
        d.map.get("kind").unwrap(),
        &Value::String("is_weather".to_string())
    );
}

#[test]
fn test_brand_decorator_validation_failure() {
    // Field-level hint rejects ill-typed payloads with `TypeMismatch`;
    // `#brand` must reach the same outcome through the same `check_type`
    // call.
    let result = eval_doc(
        r#"{
        #schema Weather: { String location: *, Int temperature: * },

        w: #brand Weather {
            location: "Shanghai",
            temperature: "hot"
        }
    }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_brand_decorator_string_form_equivalent() {
    // `#brand "Weather"` and `#brand Weather` resolve to the same
    // type name; the brand written into the dict must be identical.
    let result = eval_doc(
        r#"{
        #schema Weather: { String location: *, Int temperature: * },

        w: #brand "Weather" {
            location: "Tokyo",
            temperature: 18
        }
    }"#,
    )
    .unwrap();

    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(w) = d.map.get("w").unwrap() else {
        panic!("Expected Dict for w");
    };
    assert_eq!(w.brand.as_deref(), Some("Weather"));
}

#[test]
fn test_brand_decorator_at_document_root() {
    // The whole point of `#brand` is reaching positions where a
    // `Type field:` hint can't be written — the document root being
    // the canonical example. The schema lives in an outer scope
    // injected by the host (the parser doesn't allow a root-node
    // type hint, so this is the only way to brand a root dict).
    use std::collections::HashMap;
    let node = parse_doc(
        r#"#brand Weather {
        location: "Berlin",
        temperature: 15
    }"#,
    );
    let ctx = Context::new().with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    // Synthesize a `Weather` schema and seed it into the surrounding
    // scope so the root-level `#brand` can resolve it.
    let weather_schema = Value::Schema {
        generics: Vec::new(),
        fields: {
            let mut fields = HashMap::new();
            fields.insert(
                "location".to_string(),
                SchemaField {
                    type_hint: relon_parser::TypeNode {
                        path: vec!["String".to_string()],
                        generics: Vec::new(),
                        is_optional: false,
                        range: relon_parser::TokenRange::default(),
                        variant_fields: None,
                        doc_comment: None,
                    },
                    predicates: vec![Value::Wildcard],
                    custom_error: None,
                    default_value: None,
                },
            );
            fields.insert(
                "temperature".to_string(),
                SchemaField {
                    type_hint: relon_parser::TypeNode {
                        path: vec!["Int".to_string()],
                        generics: Vec::new(),
                        is_optional: false,
                        range: relon_parser::TokenRange::default(),
                        variant_fields: None,
                        doc_comment: None,
                    },
                    predicates: vec![Value::Wildcard],
                    custom_error: None,
                    default_value: None,
                },
            );
            fields
        },
    };
    let outer_scope = std::sync::Arc::new(Scope::default());
    outer_scope
        .locals
        .lock()
        .unwrap()
        .insert("Weather".to_string(), weather_schema);

    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval_root(&outer_scope)
        .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    assert_eq!(d.brand.as_deref(), Some("Weather"));
    assert_eq!(
        d.map.get("location").unwrap(),
        &Value::String("Berlin".to_string())
    );
    assert_eq!(d.map.get("temperature").unwrap(), &Value::Int(15));
}

#[test]
fn test_brand_directive_stacks_with_inline_schema() {
    // Stacking `#brand` on top of an inline schema declaration: the
    // schema directive seeds `Weather` into scope before the body
    // walks, so `#brand Weather { ... }` validates and brands the
    // dict in one go.
    let src = r#"{
        #schema Weather { String location: *, Int temperature: * },
        w: #brand Weather {
            location: "Paris",
            temperature: 20
        }
    }"#;
    let node = parse_doc(src);
    let ctx = fully_granted_ctx().with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval_root(&std::sync::Arc::new(Scope::default()))
        .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(w) = d.map.get("w").unwrap() else {
        panic!("Expected Dict for w");
    };
    assert_eq!(w.brand.as_deref(), Some("Weather"));
    assert_eq!(w.map.get("temperature").unwrap(), &Value::Int(20));
}

#[test]
fn test_brand_decorator_conflicts_with_field_type_hint() {
    // `Weather w: #brand Weather {...}` expresses the same intent
    // twice. Refuse the combination so the user picks one form.
    let result = eval_doc(
        r#"{
        #schema Weather: { String location: *, Int temperature: * },

        Weather w: #brand Weather {
            location: "Shanghai",
            temperature: 22
        }
    }"#,
    );
    assert!(matches!(
        result,
        Err(RuntimeError::UnsupportedOperator(msg, _)) if msg.contains("#brand")
    ));
}

#[test]
fn test_brand_decorator_unknown_schema_matches_field_form() {
    // The field-form `NotDeclared x: {...}` produces `VariableNotFound`
    // when `NotDeclared` isn't in scope (see `check_custom_schema`).
    // `#brand NotDeclared {...}` must produce the same error so the
    // two entry points stay observationally identical.
    let result = eval_doc(
        r#"{
        w: #brand NotDeclared { a: 1 }
    }"#,
    );
    assert!(matches!(result, Err(RuntimeError::VariableNotFound(_, _))));

    let field_form = eval_doc(
        r#"{
        NotDeclared w: { a: 1 }
    }"#,
    );
    assert!(matches!(
        field_form,
        Err(RuntimeError::VariableNotFound(_, _))
    ));
}

// -------- Task A: generic / optional types in `#brand ...` --------

#[test]
fn test_brand_decorator_generic_dict_validates_and_brands() {
    // `#brand Dict<String, Int>` runs `check_dict` against the value
    // (every entry's value must be `Int`) and stamps the dict with a
    // brand string that round-trips through type matches and JSON.
    // `Dict` is the canonical builtin — `Map<...>` would fall through
    // to `check_custom_schema` and fail with `VariableNotFound("Map")`
    // unless the host registers a `Map` alias schema.
    let result = eval_doc(
        r#"{
        counters: #brand Dict<String, Int> {
            hits: 1,
            misses: 7
        }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(c) = d.map.get("counters").unwrap() else {
        panic!("Expected Dict for counters");
    };
    // `Dict` is a builtin, so the brand string is the full
    // generic shape rather than the empty single-builtin case.
    assert_eq!(c.brand.as_deref(), Some("Dict<String, Int>"));
    assert_eq!(c.map.get("hits").unwrap(), &Value::Int(1));
    assert_eq!(c.map.get("misses").unwrap(), &Value::Int(7));
}

#[test]
fn test_brand_decorator_generic_dict_validation_failure() {
    // Same as the field-form `Dict<String, Int> m: { ... }` —
    // a non-Int value for any entry must reject with `TypeMismatch`.
    let result = eval_doc(
        r#"{
        counters: #brand Dict<String, Int> {
            hits: 1,
            misses: "lots"
        }
    }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_brand_decorator_generic_single_param_brand_string() {
    // `#brand Foo<T>` with no host-supplied `Foo` schema falls back to
    // `check_custom_schema` — the lookup fails on `Foo`, mirroring the
    // field form. Brand serialization for the generic shape, however,
    // is still well-defined: verify it through the lookup error path
    // by introducing `Foo` as an alias to a permissive schema first.
    let result = eval_doc(
        r#"{
        #schema Foo: { Any value: * },

        wrapped: #brand Foo<T> {
            value: 42
        }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(w) = d.map.get("wrapped").unwrap() else {
        panic!("Expected Dict for wrapped");
    };
    // `Foo<T>` serializes with its generic params for parity with the
    // field-form's `format_type_node` output.
    assert_eq!(w.brand.as_deref(), Some("Foo<T>"));
}

#[test]
fn test_brand_decorator_field_form_generic_brand_parity() {
    // The field-level type hint must produce the same brand string
    // as the decorator form for generic types. Both arrive at
    // `brand_string_for(type_node)` so a regression on either side
    // shows up here.
    let result = eval_doc(
        r#"{
        Dict<String, Int> field: { a: 1, b: 2 },
        decorated: #brand Dict<String, Int> { a: 1, b: 2 }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(f) = d.map.get("field").unwrap() else {
        panic!("Expected Dict for field");
    };
    let Value::Dict(g) = d.map.get("decorated").unwrap() else {
        panic!("Expected Dict for decorated");
    };
    assert_eq!(f.brand, g.brand);
    assert_eq!(f.brand.as_deref(), Some("Dict<String, Int>"));
}

#[test]
fn test_brand_decorator_optional_brand_string() {
    // `#brand Weather?` — the `?` suffix flows into the brand string
    // so type guards see the optionality marker. The schema validation
    // proceeds against the underlying `Weather` schema.
    let result = eval_doc(
        r#"{
        #schema Weather: { String location: *, Int temperature: * },

        w: #brand Weather? {
            location: "Tokyo",
            temperature: 18
        }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(w) = d.map.get("w").unwrap() else {
        panic!("Expected Dict for w");
    };
    assert_eq!(w.brand.as_deref(), Some("Weather?"));
}

// -------- Task B: `#brand X` at schema-field position --------

#[test]
fn test_brand_decorator_in_schema_field_equivalent_to_type_prefix() {
    // `#schema S: { #brand String name: * }` is the decorator-form
    // mirror of `#schema S: { String name: * }`. A non-`String` value
    // on the schema instance must reject with `TypeMismatch`, and a
    // `String` value must validate cleanly.
    let ok = eval_doc(
        r#"{
        #schema S: { #brand String name: * },

        S inst: { name: "Ada" }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = ok else {
        panic!("Expected Dict");
    };
    let Value::Dict(inst) = d.map.get("inst").unwrap() else {
        panic!("Expected Dict for inst");
    };
    assert_eq!(
        inst.map.get("name").unwrap(),
        &Value::String("Ada".to_string())
    );

    let bad = eval_doc(
        r#"{
        #schema S: { #brand String name: * },

        S inst: { name: 42 }
    }"#,
    );
    assert!(matches!(bad, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_brand_decorator_in_schema_field_dotted_path() {
    // `#brand geo.Location loc: *` — dotted-path arg form. The schema
    // must validate against the resolved `geo.Location` binding (here
    // re-bound in the same dict so static lookup works). Same as the
    // type-prefix form `geo.Location loc: *` — neither auto-brands
    // the nested instance; the user is expected to apply `#brand` at
    // the instance position when an explicit brand is desired.
    let result = eval_doc(
        r#"{
        geo: { Location: #schema { Number lat: *, Number lon: * } },
        #schema Place: {
            #brand geo.Location loc: *,
            String name: *
        },

        Place p: {
            loc: { lat: 1.0, lon: 2.0 },
            name: "Origin"
        }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(p) = d.map.get("p").unwrap() else {
        panic!("Expected Dict for p");
    };
    let Value::Dict(loc) = p.map.get("loc").unwrap() else {
        panic!("Expected Dict for loc");
    };
    assert_eq!(loc.map.get("lat").unwrap(), &Value::Float(1.0.into()));

    // The validation rejects an instance whose `loc` shape doesn't
    // satisfy `geo.Location` — proving the `#brand`-derived type hint
    // really drove `check_type`.
    let bad = eval_doc(
        r#"{
        geo: { Location: #schema { Number lat: *, Number lon: * } },
        #schema Place: {
            #brand geo.Location loc: *,
            String name: *
        },

        Place p: {
            loc: { lat: "north" },
            name: "Origin"
        }
    }"#,
    );
    assert!(bad.is_err(), "expected validation failure, got {:?}", bad);
}

#[test]
fn test_brand_decorator_in_schema_field_generic_dict() {
    // `#brand Dict<String, Int> m: *` lifts the generic type into the
    // field's type hint; instances with non-Int values must reject.
    let ok = eval_doc(
        r#"{
        #schema Counters: {
            #brand Dict<String, Int> m: *
        },

        Counters c: { m: { a: 1, b: 2 } }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = ok else {
        panic!("Expected Dict");
    };
    let Value::Dict(c) = d.map.get("c").unwrap() else {
        panic!("Expected Dict for c");
    };
    assert!(c.map.contains_key("m"));

    let bad = eval_doc(
        r#"{
        #schema Counters: {
            #brand Dict<String, Int> m: *
        },

        Counters c: { m: { a: 1, b: "two" } }
    }"#,
    );
    assert!(matches!(bad, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn test_brand_decorator_in_schema_field_conflict_with_explicit_type() {
    // When a schema field carries BOTH an explicit type prefix AND a
    // `#brand ...` decorator, the analyzer flags it as a conflict.
    // Source: `#brand Bar Foo x: *` — both forms try to declare `x`'s
    // type. Verify the diagnostic shape directly via `relon_analyzer`.
    let node = parse_doc(
        r#"{
        #schema S: { #brand Bar Foo x: * }
    }"#,
    );
    let tree = relon_analyzer::analyze(&node);
    assert!(
        tree.diagnostics.iter().any(|d| matches!(
            d,
            relon_analyzer::Diagnostic::SchemaFieldBrandConflict { field, .. } if field == "x"
        )),
        "expected SchemaFieldBrandConflict diagnostic, got {:?}",
        tree.diagnostics
    );
}

#[test]
fn test_brand_decorator_in_schema_field_does_not_emit_untyped_warning() {
    // `#brand X y: *` is a complete field declaration — the schema
    // pass must NOT emit `SchemaFieldUntyped` for it.
    let node = parse_doc(
        r#"{
        #schema S: { #brand String name: * }
    }"#,
    );
    let tree = relon_analyzer::analyze(&node);
    assert!(
        !tree
            .diagnostics
            .iter()
            .any(|d| matches!(d, relon_analyzer::Diagnostic::SchemaFieldUntyped { .. })),
        "should not emit SchemaFieldUntyped for `#brand`-typed field, got {:?}",
        tree.diagnostics
    );
}

#[test]
fn test_brand_decorator_in_schema_field_end_to_end_with_meta() {
    // End-to-end: a `#brand ...`-typed field still composes with the
    // other schema-field meta decorators (`#expect`, `#default`).
    // The custom error must surface on a missing field; the default
    // must populate a missing field in the instance.
    let result = eval_doc(
        r#"{
        #schema User: {
            #brand String name: *,
            #default 0 #brand Int age: *
        },

        User alice: { name: "Alice" }
    }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(alice) = d.map.get("alice").unwrap() else {
        panic!("Expected Dict for alice");
    };
    assert_eq!(
        alice.map.get("name").unwrap(),
        &Value::String("Alice".to_string())
    );
    assert_eq!(alice.map.get("age").unwrap(), &Value::Int(0));
}

#[test]
fn test_schema_defaulting() {
    let result = eval_doc(
        r#"{
        #schema User: {
            #default "guest"
            String role: *,
            Int age: *
        },
        
        User alice: { age: 25 }
    }"#,
    )
    .unwrap();

    if let Value::Dict(d) = result {
        let alice = d.map.get("alice").unwrap();
        if let Value::Dict(alice_d) = alice {
            assert_eq!(
                alice_d.map.get("role").unwrap(),
                &Value::String("guest".to_string())
            );
            assert_eq!(alice_d.map.get("age").unwrap(), &Value::Int(25));
        } else {
            panic!("Expected Dict for alice");
        }
    } else {
        panic!("Expected Dict");
    }
}

#[test]
fn test_schema_computed_default_from_siblings() {
    // A closure passed to #default fires at validation time with the
    // partially-populated instance bound to `self`, computing the field
    // from sibling values.
    let result = eval_doc(
        r#"{
        #schema User: {
            String first: *,
            String last: *,
            #default (self) => self.first + " " + self.last
            String full: *
        },

        User u: { first: "Ada", last: "Lovelace" }
    }"#,
    )
    .unwrap();

    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(u) = d.map.get("u").unwrap() else {
        panic!("Expected Dict for u");
    };
    assert_eq!(
        u.map.get("full").unwrap(),
        &Value::String("Ada Lovelace".to_string())
    );
}

#[test]
fn test_schema_computed_default_does_not_override_explicit_value() {
    // When the field is explicitly provided, the closure default must
    // not fire — explicit value wins, same as literal defaults.
    let result = eval_doc(
        r#"{
        #schema User: {
            String first: *,
            String last: *,
            #default (self) => self.first + " " + self.last
            String full: *
        },

        User u: { first: "Ada", last: "Lovelace", full: "Countess" }
    }"#,
    )
    .unwrap();

    let Value::Dict(d) = result else {
        panic!("Expected Dict");
    };
    let Value::Dict(u) = d.map.get("u").unwrap() else {
        panic!("Expected Dict for u");
    };
    assert_eq!(
        u.map.get("full").unwrap(),
        &Value::String("Countess".to_string())
    );
}

#[test]
fn test_analyzer_target_agrees_with_evaluator_resolution() {
    // For a sibling reference, the evaluator's runtime resolution
    // must produce a value that originates from the very node the
    // analyzer's `references` table points to. We check this by
    // comparing the resolved target's evaluation against the
    // reference site's evaluation — they should be equal.
    let source = r#"{ a: 10 + 5, b: &sibling.a }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));

    // Find the reference site `&sibling.a`'s NodeId.
    let Expr::Dict(pairs) = &*node.expr else {
        panic!()
    };
    let (_, b_value) = pairs
        .iter()
        .find(|(k, _)| k.to_string_key() == "b")
        .expect("field b");
    let ref_site_id = b_value.id;

    // Evaluate normally.
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(analyzed.clone());
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let result = eval
        .eval_root(&std::sync::Arc::new(Scope::default()))
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    let b_val = d.map.get("b").unwrap().clone();

    // Pull the analyzer's bound target and evaluate it directly;
    // the two values must agree.
    let target_node = eval
        .context
        .analyzer_target(ref_site_id)
        .expect("analyzer should bind &sibling.a");
    let target_val = eval
        .eval(&target_node, &std::sync::Arc::new(Scope::default()))
        .unwrap();
    assert_eq!(b_val, target_val);
}

#[test]
fn test_forward_lookahead_next() {
    let result = eval_doc(
        r#"[
        { title: "Step 1", next_is_final: &next.final },
        { title: "Step 2", final: true }
    ]"#,
    )
    .unwrap();

    if let Value::List(l) = result {
        if let Value::Dict(ref m0) = l[0] {
            assert_eq!(m0.map.get("next_is_final").unwrap(), &Value::Bool(true));
        } else {
            panic!("Expected Dict at index 0");
        }
    } else {
        panic!("Expected List");
    }
}

// ---------- Tagged-enum (sum-type) tests ----------

#[test]
fn variant_ctor_constructs_branded_dict() {
    let result = eval_doc(
        r#"{
            #schema Notification Enum<
                Email { address: String, subject: String },
                Push,
            >,
            msg: Notification.Email { address: "a@b.c", subject: "hi" }
        }"#,
    )
    .unwrap();
    let Value::Dict(outer) = result else {
        panic!("dict")
    };
    let Value::Dict(msg) = outer.map.get("msg").unwrap() else {
        panic!("msg dict")
    };
    assert_eq!(msg.brand.as_deref(), Some("Email"));
    assert_eq!(msg.variant_of.as_deref(), Some("Notification"));
    assert_eq!(
        msg.map.get("address").unwrap(),
        &Value::String("a@b.c".to_string())
    );
}

#[test]
fn variant_ctor_unit_variant_works() {
    let result = eval_doc(
        r#"{
            #schema Notification Enum<
                Email { address: String },
                Push,
            >,
            msg: Notification.Push {}
        }"#,
    )
    .unwrap();
    let Value::Dict(outer) = result else { panic!() };
    let Value::Dict(msg) = outer.map.get("msg").unwrap() else {
        panic!()
    };
    assert_eq!(msg.brand.as_deref(), Some("Push"));
    assert_eq!(msg.variant_of.as_deref(), Some("Notification"));
    assert!(msg.map.is_empty());
}

#[test]
fn variant_ctor_unknown_variant_runtime_error() {
    let result = eval_doc(
        r#"{
            #schema N Enum<A { x: Int }>,
            msg: N.Bogus { x: 1 }
        }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn variant_ctor_missing_required_field_errors() {
    let result = eval_doc(
        r#"{
            #schema N Enum<Email { address: String, subject: String }>,
            msg: N.Email { address: "a@b" }
        }"#,
    );
    assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
}

#[test]
fn variant_value_field_access_is_flat() {
    // `msg.address` reads the variant's payload field directly with
    // no `.Email.` indirection — same access path as a plain dict.
    let result = eval_doc(
        r#"{
            #schema N Enum<Email { address: String }>,
            msg: N.Email { address: "a@b.c" },
            got: &sibling.msg.address
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("got").unwrap(),
        &Value::String("a@b.c".to_string())
    );
}

#[test]
fn match_on_variant_dispatches_via_brand() {
    let result = eval_doc(
        r#"{
            #schema N Enum<
                Email { address: String },
                Push,
            >,
            msg: N.Email { address: "a@b.c" },
            out: msg match {
                Email: f"emailed ${msg.address}",
                Push:  "push"
            }
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("out").unwrap(),
        &Value::String("emailed a@b.c".to_string())
    );
}

#[test]
fn untagged_enum_string_literal_still_validates() {
    // Regression: classic `Enum<"up", "down">` must keep working.
    let result = eval_doc(
        r#"{
            #schema Status: { String mode: Enum<"up", "down"> },
            Status s: { mode: "up" }
        }"#,
    );
    assert!(result.is_ok(), "{:?}", result);
}

#[test]
fn untagged_enum_type_set_still_validates() {
    let result = eval_doc(
        r#"{
            #schema Theme: { id: Enum<Int, String> },
            Theme t: { id: 7 }
        }"#,
    );
    assert!(result.is_ok(), "{:?}", result);
}

#[test]
fn run_main_validates_args_and_fills_defaults() {
    // `#main(Req req)` declares the file as an entry program with one
    // named parameter. Host pushes the matching arg as a HashMap; the
    // missing schema field carries `#default 0` so validation accepts
    // the input and materializes the default.
    use std::collections::{BTreeMap, HashMap};
    let source = r#"#schema Req {
    String name: *,
    #default 0
    Int retries: *
}
#main(Req req)
{ greeting: f"hello ${req.name}, retries=${req.retries}" }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    assert!(analyzed.main_signature.is_some());
    assert_eq!(analyzed.main_signature.as_ref().unwrap().params.len(), 1);

    let mut req = BTreeMap::new();
    req.insert("name".to_string(), Value::String("Alice".to_string()));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("req".to_string(), Value::dict(req));

    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args)
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("greeting").unwrap(),
        &Value::String("hello Alice, retries=0".to_string())
    );
}

#[test]
fn run_main_with_multiple_params() {
    // Multi-parameter `#main(...)` binds each arg into the root scope's
    // locals — no `input.` prefix. References to each parameter from
    // the body resolve directly through scope.
    use std::collections::{BTreeMap, HashMap};
    let source = r#"#schema User { String name: * }
#schema Cart { Int total: * }
#main(User user, Cart cart)
{ summary: f"${user.name} - ${cart.total}" }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    assert!(analyzed.main_signature.is_some());

    let mut user = BTreeMap::new();
    user.insert("name".to_string(), Value::String("Alice".to_string()));
    let mut cart = BTreeMap::new();
    cart.insert("total".to_string(), Value::Int(100));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("user".to_string(), Value::dict(user));
    args.insert("cart".to_string(), Value::dict(cart));

    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args)
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("summary").unwrap(),
        &Value::String("Alice - 100".to_string())
    );
}

#[test]
fn run_main_missing_arg_errors() {
    // Host fails to push a value for a declared parameter — surface a
    // `MissingMainArg` immediately.
    use std::collections::HashMap;
    let source = r#"#schema Req { String name: * }
#main(Req req)
{ greeting: req.name }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), HashMap::new());
    assert!(
        matches!(&result, Err(RuntimeError::MissingMainArg { name, .. }) if name == "req"),
        "expected MissingMainArg(req), got {result:?}"
    );
}

#[test]
fn run_main_unexpected_arg_errors() {
    // Host pushes a name not declared by `#main(...)`. The signature is
    // strict — extras are rejected so the host catches typos early.
    use std::collections::HashMap;
    let source = r#"#schema Req { String name: * }
#main(Req req)
{ ok: 1 }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let mut args: HashMap<String, Value> = HashMap::new();
    let mut req = std::collections::BTreeMap::new();
    req.insert("name".to_string(), Value::String("A".to_string()));
    args.insert("req".to_string(), Value::dict(req));
    args.insert("bogus".to_string(), Value::Int(0));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args);
    assert!(
        matches!(&result, Err(RuntimeError::UnexpectedMainArg { name, .. }) if name == "bogus"),
        "expected UnexpectedMainArg(bogus), got {result:?}"
    );
}

#[test]
fn run_main_arg_type_mismatch_errors() {
    // Pushed value's type doesn't match the declared parameter type.
    // Surface `MainArgTypeMismatch` rather than letting the body
    // explode mid-evaluation.
    use std::collections::HashMap;
    let source = r#"#main(Int n)
{ ok: n + 1 }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::String("not an int".to_string()));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args);
    assert!(
        matches!(&result, Err(RuntimeError::MainArgTypeMismatch { name, .. }) if name == "n"),
        "expected MainArgTypeMismatch(n), got {result:?}"
    );
}

#[test]
fn run_main_without_signature_errors() {
    // File without `#main(...)` cannot be `run_main`-ed; libraries /
    // static configs use `eval_root` instead.
    use std::collections::HashMap;
    let source = r#"{ ok: 1 }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    assert!(analyzed.main_signature.is_none());
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), HashMap::new());
    assert!(
        matches!(&result, Err(RuntimeError::NoMainSignature { .. })),
        "expected NoMainSignature, got {result:?}"
    );
}

#[test]
fn duplicate_main_directive_is_an_analyzer_error() {
    // A file may declare at most one `#main(...)`; later declarations
    // are flagged as `DuplicateMainDirective`.
    let node = parse_doc(
        r#"#main(Int a)
#main(Int b)
{ ok: 1 }"#,
    );
    let analyzed = relon_analyzer::analyze(&node);
    assert!(analyzed.has_errors());
    assert!(
        analyzed
            .diagnostics
            .iter()
            .any(|d| matches!(d, relon_analyzer::Diagnostic::DuplicateMainDirective { .. })),
        "expected DuplicateMainDirective, got {:?}",
        analyzed.diagnostics
    );
}

#[test]
fn private_field_is_dropped_from_dict_map_but_visible_to_siblings() {
    // `#private` keeps a binding alive in the owning dict's locals so
    // siblings can reference it, while excluding it from the produced
    // `Value::Dict::map`. Net effect: `display` resolves correctly,
    // but the consumer never sees `helper` in the output.
    let result = eval_doc(
        r#"{
            #private
            helper(v): "<" + v + ">",
            display: helper("hi")
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("display").unwrap(),
        &Value::String("<hi>".to_string())
    );
    assert!(
        !d.map.contains_key("helper"),
        "private field leaked into map: {:?}",
        d.map.keys().collect::<Vec<_>>()
    );
}

#[test]
fn private_value_field_is_also_dropped_from_dict_map() {
    // The marker isn't closure-specific: any field type — including
    // plain strings — disappears from the map when `#private`.
    let result = eval_doc(
        r#"{
            #private
            secret: "shhh",
            public: &sibling.secret + " (declassified)"
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("public").unwrap(),
        &Value::String("shhh (declassified)".to_string())
    );
    assert!(!d.map.contains_key("secret"));
}

#[test]
fn private_field_is_not_visible_through_alias_import() {
    // After `#import lib from "lib"`, only fields present in the
    // module's exported `Value::Dict::map` are reachable. Private
    // fields aren't, so `lib.secret` fails with `VariableNotFound`.
    let dir = std::env::temp_dir().join(format!("relon-private-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.relon"),
        r#"{
            #private
            secret: "shhh",
            public: "ok"
        }"#,
    )
    .unwrap();

    let node = parse_doc(
        r#"#import lib from "./lib.relon"
            { leak: lib.secret }"#,
    );
    let ctx = fully_granted_ctx().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let scope = std::sync::Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&scope);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        matches!(&result, Err(RuntimeError::VariableNotFound(name, _)) if name == "lib.secret"),
        "expected VariableNotFound(lib.secret), got {result:?}"
    );
}

#[test]
fn private_field_is_skipped_by_import_spread() {
    // `@import(..., spread=true)` copies the module's exported keys
    // into the importing scope. Private fields aren't in that export,
    // so they don't appear in the spread either.
    let dir = std::env::temp_dir().join(format!("relon-private-spread-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.relon"),
        r#"{
            #private
            internal: "kept inside",
            exported: "out"
        }"#,
    )
    .unwrap();

    let node = parse_doc(
        r#"#import * from "./lib.relon"
            { has_exported: exported, has_internal: internal }"#,
    );
    let ctx = fully_granted_ctx().with_root(node.clone());
    let ctx = std::sync::Arc::new(ctx);
    let scope = std::sync::Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&scope);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        matches!(&result, Err(RuntimeError::VariableNotFound(name, _)) if name == "internal"),
        "expected VariableNotFound(internal), got {result:?}"
    );
}

#[test]
fn dynamic_key_error_propagates_from_prepare_phase() {
    // A dynamic key whose expression fails to evaluate used to be
    // silently skipped during thunk registration (the prepare phase
    // would `_ => continue`), so the caller would re-evaluate the
    // same expression in the dict-emit phase and only *then* see
    // the error. That re-evaluation is wasted work and the prepare
    // invariant ("thunks cover every declared key") was a lie.
    //
    // Now the error propagates straight out of prepare. The exact
    // RuntimeError variant depends on what failed; here the dynamic
    // key references an undefined variable, so we expect
    // `VariableNotFound`.
    let result = eval_doc(
        r#"{
            [missing_var]: 1
        }"#,
    );
    assert!(
        matches!(&result, Err(RuntimeError::VariableNotFound(name, _)) if name == "missing_var"),
        "expected VariableNotFound(missing_var), got {result:?}"
    );
}

#[test]
fn underscore_prefix_no_longer_implies_private() {
    // The legacy `_xxx` convention is gone: a field whose name happens
    // to start with `_` is now an ordinary field. (Identifiers may
    // still start with `_` — that's a parser-level rule, unrelated to
    // visibility.) Verifies the `_` rule is fully retired.
    let result = eval_doc(
        r#"{
            _legacy: "still here",
            shown: &sibling._legacy
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("_legacy").unwrap(),
        &Value::String("still here".to_string())
    );
    assert_eq!(
        d.map.get("shown").unwrap(),
        &Value::String("still here".to_string())
    );
}

#[test]
fn root_schema_directive_validates_main_arg() {
    // `#schema User ...` at root level seeds `User` into scope; the
    // following `#main(User req)` references it the same way as a
    // dict-field `#schema User ...` would.
    use std::collections::{BTreeMap, HashMap};
    let source = r#"#schema User { String name: *, Int age: * }
#main(User req)
{ greeting: f"hello ${req.name}, age=${req.age}" }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    assert_eq!(analyzed.root_schemas.len(), 1);
    assert_eq!(analyzed.root_schemas[0].name, "User");
    assert!(analyzed.main_signature.is_some());

    let mut req = BTreeMap::new();
    req.insert("name".to_string(), Value::String("Alice".to_string()));
    req.insert("age".to_string(), Value::Int(30));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("req".to_string(), Value::dict(req));

    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args)
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("greeting").unwrap(),
        &Value::String("hello Alice, age=30".to_string())
    );
}

#[test]
fn root_schema_directive_supports_multiple_declarations() {
    // Stack two `#schema A ...` directives at the root level — each
    // `#main(...)` parameter resolves through the merged scope.
    use std::collections::{BTreeMap, HashMap};
    let source = r#"#schema User { String name: * }
#schema Cart { Int total: * }
#main(User user, Cart cart)
{ summary: f"${user.name} - ${cart.total}" }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    assert_eq!(analyzed.root_schemas.len(), 2);

    let mut user = BTreeMap::new();
    user.insert("name".to_string(), Value::String("Alice".to_string()));
    let mut cart = BTreeMap::new();
    cart.insert("total".to_string(), Value::Int(100));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("user".to_string(), Value::dict(user));
    args.insert("cart".to_string(), Value::dict(cart));

    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args)
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("summary").unwrap(),
        &Value::String("Alice - 100".to_string())
    );
}

#[test]
fn root_schema_directive_visible_inside_dict_body() {
    // Schemas declared via `#schema Name ...` at the root must also
    // resolve from inside the dict body — `Name { ... }` should bind
    // to the same `Value::Schema` the analyzer registers.
    let source = r#"#schema User { String name: *, Int age: * }
{
    User alice: { name: "Alice", age: 30 }
}"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .eval_root(&std::sync::Arc::new(Scope::default()))
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    let Value::Dict(alice) = d.map.get("alice").unwrap() else {
        panic!("expected dict");
    };
    assert_eq!(
        alice.map.get("name").unwrap(),
        &Value::String("Alice".to_string())
    );
    assert_eq!(alice.map.get("age").unwrap(), &Value::Int(30));
}

#[test]
fn duplicate_root_schema_name_is_an_analyzer_error() {
    // Two `#schema A ...` directives claiming the same schema name
    // would shadow each other; reject up front.
    let node = parse_doc(
        r#"#schema User { String name: * }
#schema User { Int age: * }
{ greeting: "hi" }"#,
    );
    let analyzed = relon_analyzer::analyze(&node);
    assert!(analyzed.has_errors());
    assert!(
        analyzed
            .diagnostics
            .iter()
            .any(|d| matches!(d, relon_analyzer::Diagnostic::DuplicateRootSchemaName { name, .. } if name == "User")),
        "expected DuplicateRootSchemaName(User), got {:?}",
        analyzed.diagnostics
    );
}

#[test]
fn root_schema_invalid_value_type_is_an_analyzer_error() {
    // `#schema Foo 42` — the body isn't a schema body. Static reject.
    let node = parse_doc(
        r#"#schema Foo 42
{ greeting: "hi" }"#,
    );
    let analyzed = relon_analyzer::analyze(&node);
    assert!(analyzed.has_errors());
    assert!(
        analyzed
            .diagnostics
            .iter()
            .any(|d| matches!(d, relon_analyzer::Diagnostic::RootSchemaInvalidValue { name, .. } if name == "Foo")),
        "expected RootSchemaInvalidValue(Foo), got {:?}",
        analyzed.diagnostics
    );
}

#[test]
fn user_defined_decorator_applies_closure_to_value() {
    // `@f` looks up `f` as a callable in scope when no built-in plugin
    // matches. `@upper "hello"` ≡ `upper("hello")`, where `upper` is a
    // closure defined as a sibling.
    let result = eval_doc(
        r#"{
            #private
            shout(s): s + "!",
            greeting: @shout "hello"
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("greeting").unwrap(),
        &Value::String("hello!".to_string())
    );
}

#[test]
fn user_defined_decorator_with_extra_args() {
    // `@f(a, b)` on `v` ≡ `f(v, a, b)`. Args after the decorated value
    // are appended to the call.
    let result = eval_doc(
        r#"{
            #private
            wrap(s, l, r): l + s + r,
            display: @wrap("[", "]") "core"
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(
        d.map.get("display").unwrap(),
        &Value::String("[core]".to_string())
    );
}

#[test]
fn user_defined_decorator_stack_is_innermost_first() {
    // `@a @b v` ≡ `a(b(v))` — bottom-up, nearest-to-value first.
    let result = eval_doc(
        r#"{
            #private
            paren(s): "(" + s + ")",
            #private
            star(s): "*" + s + "*",
            display: @star @paren "x"
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    // bottom-up: paren("x") = "(x)", star("(x)") = "*(x)*"
    assert_eq!(
        d.map.get("display").unwrap(),
        &Value::String("*(x)*".to_string())
    );
}

// ----- Review-fix regressions ---------------------------------------------

#[test]
fn closure_call_does_not_share_path_cache_across_invocations() {
    // P1-A regression: a closure body references a sibling binding
    // (`&sibling.a`). When the closure is called twice with different
    // arguments, the per-invocation `cache_namespace` must isolate path
    // caching so the second call doesn't reuse the first call's `a`.
    let result = eval_doc(
        r#"{
            make(x): { a: x, b: &sibling.a },
            d1: make(1),
            d2: make(2)
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    let Some(Value::Dict(d1)) = d.map.get("d1") else {
        panic!("d1 not dict")
    };
    let Some(Value::Dict(d2)) = d.map.get("d2") else {
        panic!("d2 not dict")
    };
    assert_eq!(d1.map.get("b"), Some(&Value::Int(1)), "d1.b should be 1");
    assert_eq!(
        d2.map.get("b"),
        Some(&Value::Int(2)),
        "d2.b should be 2 (path cache must not bleed across closure calls)"
    );
}

#[test]
fn dynamic_segment_reference_cache_keys_do_not_collide() {
    // P1-B regression: `&sibling.obj[&sibling.k1]` and
    // `&sibling.obj[&sibling.k2]` previously hashed identically because
    // both dynamic segments stringified to "<dynamic>". The fix evaluates
    // the dynamic key when forming the cache key, so each lookup hits
    // (or misses) under its own real key.
    let result = eval_doc(
        r#"{
            obj: { a: 1, b: 2 },
            k1: "a",
            k2: "b",
            v1: &sibling.obj[&sibling.k1],
            v2: &sibling.obj[&sibling.k2]
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(d.map.get("v1"), Some(&Value::Int(1)));
    assert_eq!(d.map.get("v2"), Some(&Value::Int(2)));
}

#[test]
fn dynamic_segment_resolves_against_materialized_dict() {
    // P2-A regression: when a reference passes through a value that has
    // already been materialized (e.g. the result of a function call), the
    // remaining dynamic segments must be evaluated against the call's
    // scope, not looked up as the literal string "<dynamic>".
    let result = eval_doc(
        r#"{
            id(v): v,
            obj: id({ x: 10, y: 20 }),
            k: "y",
            val: &sibling.obj[&sibling.k]
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(d.map.get("val"), Some(&Value::Int(20)));
}

#[test]
fn next_reference_target_carries_its_own_list_context() {
    // P2-B regression: `&next.y` forces the adjacent list element. The
    // forced element's body uses `&index`, which only resolves inside a
    // list scope. The fix builds a list scope for the target index when
    // forcing, so `&index` in the second element is reachable.
    let result = eval_doc(
        r#"[
            { x: &next.y },
            { y: &index }
        ]"#,
    )
    .unwrap();
    let Value::List(items) = result else { panic!() };
    assert_eq!(items.len(), 2);
    let Value::Dict(first) = &items[0] else {
        panic!()
    };
    assert_eq!(first.map.get("x"), Some(&Value::Int(1)));
    let Value::Dict(second) = &items[1] else {
        panic!()
    };
    assert_eq!(second.map.get("y"), Some(&Value::Int(1)));
}

#[test]
fn private_field_is_not_visible_through_root_reference() {
    // P2-C regression: a `#private` field on the root dict must not be
    // reachable via `&root.<name>` from a nested dict — the field's
    // visibility is local to its owning dict's siblings.
    let result = eval_doc(
        r#"{
            #private
            secret: "shhh",
            child: { leak: &root.secret }
        }"#,
    );
    assert!(
        matches!(&result, Err(RuntimeError::VariableNotFound(name, _)) if name.contains("secret")),
        "expected VariableNotFound for cross-dict private access, got {result:?}"
    );
}

#[test]
fn imported_module_runs_analyzer_and_surfaces_errors() {
    // P2-E regression: a module with a structural analyzer error
    // (here a schema field missing its type annotation) used to be
    // silently accepted by `load_module` because only `parse_document`
    // ran. The fix runs the analyzer and refuses to evaluate when any
    // diagnostic has Error severity.
    let dir = std::env::temp_dir().join(format!("relon-mod-analyze-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bad.relon"),
        // `name: *` without a type annotation — analyzer flags it.
        r#"#schema Bad { name: * }
{ exported: 1 }"#,
    )
    .unwrap();

    let node = parse_doc(
        r#"#import bad from "./bad.relon"
            { x: bad.exported }"#,
    );
    let ctx = fully_granted_ctx().with_root(node.clone());
    let scope = std::sync::Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(std::sync::Arc::new(ctx)).eval_root(&scope);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        matches!(&result, Err(RuntimeError::ModuleParseError { .. })),
        "expected ModuleParseError for a module with analyzer errors, got {result:?}"
    );
}

#[test]
fn step_counter_resets_between_top_level_runs() {
    // P2-F regression: `Context::step_counter` is monotonic. Reusing the
    // same Evaluator/Context across two top-level evaluations used to
    // accumulate the prior step count — the second run could trip the
    // budget even though it's small. `eval_root` and `run_main` now
    // reset the counter on entry.
    let node = parse_doc(r#"{ a: 1, b: 2, c: 3 }"#);
    let mut ctx = Context::new().with_root(node.clone());
    // Tight budget that fits one such evaluation but would overflow on
    // accumulated steps from two back-to-back runs.
    ctx.capabilities.max_steps = Some(50);
    let ctx = std::sync::Arc::new(ctx);
    let eval = Evaluator::new(std::sync::Arc::clone(&ctx));
    let scope = std::sync::Arc::new(Scope::default());

    let r1 = eval.eval_root(&scope);
    assert!(r1.is_ok(), "first run should fit in budget: {r1:?}");
    let r2 = eval.eval_root(&scope);
    assert!(
        r2.is_ok(),
        "second run should also fit (counter reset): {r2:?}"
    );
}

#[test]
fn run_main_return_type_mismatch_errors() {
    // `#main(...) -> Type` regression: when the body's value doesn't
    // satisfy the declared return type, surface `MainReturnTypeMismatch`
    // rather than letting the host see an untyped value.
    use std::collections::HashMap;
    let source = r#"#main(Int n) -> String
{ result: n + 1 }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    assert!(analyzed.main_signature.is_some());
    assert!(analyzed
        .main_signature
        .as_ref()
        .unwrap()
        .return_type
        .is_some());

    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(7));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args);
    assert!(
        matches!(&result, Err(RuntimeError::MainReturnTypeMismatch { expected, .. }) if expected == "String"),
        "expected MainReturnTypeMismatch(String), got {result:?}"
    );
}

#[test]
fn run_main_return_type_match_passes() {
    // Counterpart to the mismatch case: when the body satisfies the
    // declared return type, `run_main` returns the value as usual.
    use std::collections::HashMap;
    let source = r#"#main(Int n) -> Dict
{ result: n + 1 }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));

    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(7));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args)
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(d.map.get("result"), Some(&Value::Int(8)));
}

// ---------- Generic schema tests (Phase 7) ----------

#[test]
fn user_defined_generic_enum_schema_lowers_with_generics() {
    // `#schema Box<T> Enum<Wrap { value: T }>` lowers to a
    // `Value::EnumSchema` whose `generics` vector carries the
    // declared parameter names. The variant's payload type still
    // mentions the bare `T` (substitution happens at the use site).
    let result = eval_doc(
        r#"{
            #schema Box<T> Enum<Wrap { value: T }>,
            value: Box.Wrap { value: 42 }
        }"#,
    )
    .unwrap();
    let Value::Dict(outer) = result else {
        panic!("dict")
    };
    let Value::Dict(b) = outer.map.get("value").unwrap() else {
        panic!("variant dict")
    };
    assert_eq!(b.brand.as_deref(), Some("Wrap"));
    assert_eq!(b.variant_of.as_deref(), Some("Box"));
    assert_eq!(b.map.get("value"), Some(&Value::Int(42)));
}

#[test]
fn builtin_result_schema_is_seeded_at_startup() {
    // `Result.Ok { ... }` is reachable without any user-side
    // `#schema Result<...>` declaration — the prelude seeds it into
    // `Context.schemas` at construction time.
    use std::collections::HashMap;
    let source = r#"#main(Int n) -> Dict
{ ok: Result.Ok { value: n } }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(42));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let result = Evaluator::new(std::sync::Arc::new(ctx))
        .run_main(&std::sync::Arc::new(Scope::default()), args)
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    let Value::Dict(ok) = d.map.get("ok").unwrap() else {
        panic!()
    };
    assert_eq!(ok.brand.as_deref(), Some("Ok"));
    assert_eq!(ok.variant_of.as_deref(), Some("Result"));
    assert_eq!(ok.map.get("value"), Some(&Value::Int(42)));
}

#[test]
fn builtin_option_some_seeds_at_startup() {
    // Option<T> is the second prelude entry. Same pre-seeded path —
    // `Option.Some { value: x }` works without any declaration.
    let result = eval_doc(
        r#"{
            v: Option.Some { value: 7 }
        }"#,
    )
    .unwrap();
    let Value::Dict(outer) = result else { panic!() };
    let Value::Dict(some) = outer.map.get("v").unwrap() else {
        panic!()
    };
    assert_eq!(some.brand.as_deref(), Some("Some"));
    assert_eq!(some.variant_of.as_deref(), Some("Option"));
    assert_eq!(some.map.get("value"), Some(&Value::Int(7)));
}

#[test]
fn builtin_option_none_unit_variant_works() {
    // `Option.None {}` is a unit variant — empty body, no payload.
    let result = eval_doc(
        r#"{
            v: Option.None {}
        }"#,
    )
    .unwrap();
    let Value::Dict(outer) = result else { panic!() };
    let Value::Dict(none) = outer.map.get("v").unwrap() else {
        panic!()
    };
    assert_eq!(none.brand.as_deref(), Some("None"));
    assert_eq!(none.variant_of.as_deref(), Some("Option"));
    assert!(none.map.is_empty());
}

#[test]
fn generic_result_payload_type_is_checked_via_schema_field() {
    // Field-level type hint `Result<Int, String> r: *` substitutes
    // `T -> Int` when validating the variant payload. A `String`
    // value where `T == Int` must be rejected.
    let result = eval_doc(
        r#"{
            Result<Int, String> r: Result.Ok { value: "oops" }
        }"#,
    );
    assert!(
        matches!(&result, Err(RuntimeError::TypeMismatch { .. })),
        "expected TypeMismatch on String payload for T=Int, got {result:?}"
    );
}

#[test]
fn generic_result_payload_type_accepts_matching_type() {
    // Counterpart: `Ok { value: Int }` matches `Result<Int, String>`'s
    // `T -> Int` substitution and the field-level check passes.
    // (The outer field's type hint also rebrands `r` with the generic
    // type's stringified form — that's standard `Type field: value`
    // behavior and not specific to this feature.)
    let result = eval_doc(
        r#"{
            Result<Int, String> r: Result.Ok { value: 99 }
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    let Value::Dict(ok) = d.map.get("r").unwrap() else {
        panic!("expected dict for r")
    };
    // Payload value made it through unchanged. The variant_of marker
    // is preserved even after the outer brand override.
    assert_eq!(ok.variant_of.as_deref(), Some("Result"));
    assert_eq!(ok.map.get("value"), Some(&Value::Int(99)));
}

#[test]
fn generic_option_field_type_substitutes_payload() {
    // Same pattern with the prelude `Option<T>` schema — `T -> Int`
    // substitution, a `String` payload must error out.
    let result = eval_doc(
        r#"{
            Option<Int> v: Option.Some { value: "no" }
        }"#,
    );
    assert!(
        matches!(&result, Err(RuntimeError::TypeMismatch { .. })),
        "expected TypeMismatch for String value where T=Int, got {result:?}"
    );
}

#[test]
fn user_can_override_prelude_option_schema() {
    // A user-defined `#schema Option ...` should shadow the prelude's
    // entry. Prove it by giving Option a non-prelude variant `Many`.
    let result = eval_doc(
        r#"{
            #schema Option Enum<Many { items: List }>,
            v: Option.Many { items: [1, 2] }
        }"#,
    )
    .unwrap();
    let Value::Dict(outer) = result else { panic!() };
    let Value::Dict(many) = outer.map.get("v").unwrap() else {
        panic!()
    };
    assert_eq!(many.brand.as_deref(), Some("Many"));
}

// =====================================================================
// Bug regressions found during second-round review.
// =====================================================================

/// Bug 1: `path_cache` lives on `Context` but its keys don't include
/// `#main` arguments, so reusing one Evaluator across multiple
/// `run_main` invocations used to hand back the previous run's cached
/// reference-path values. Both `eval_root` and `run_main` now clear the
/// cache on entry; verify by running the same program twice with
/// different args and asserting the second run sees fresh values.
#[test]
fn run_main_path_cache_isolated_across_invocations() {
    use std::collections::HashMap;
    let source = r#"#main(Int n) -> Dict
{ a: n, b: &sibling.a }"#;
    let node = parse_doc(source);
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node.clone())
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let evaluator = Evaluator::new(std::sync::Arc::new(ctx));

    let mut args1: HashMap<String, Value> = HashMap::new();
    args1.insert("n".to_string(), Value::Int(1));
    let r1 = evaluator
        .run_main(&std::sync::Arc::new(Scope::default()), args1)
        .unwrap();
    let Value::Dict(d1) = r1 else { panic!() };
    assert_eq!(d1.map.get("b"), Some(&Value::Int(1)));

    let mut args2: HashMap<String, Value> = HashMap::new();
    args2.insert("n".to_string(), Value::Int(2));
    let r2 = evaluator
        .run_main(&std::sync::Arc::new(Scope::default()), args2)
        .unwrap();
    let Value::Dict(d2) = r2 else { panic!() };
    assert_eq!(
        d2.map.get("b"),
        Some(&Value::Int(2)),
        "second run must not hit first run's cached b"
    );
}

/// Bug 2: a `#private` field is invisible from `Value::Dict::map`, but
/// dict evaluation also seeds the value into `dict_scope.locals` so
/// same-dict siblings can reach it. A previous fix already taught
/// `resolve_dict_reference_step` to hide private fields when the
/// access crosses a dict boundary, but the caller's `scope.locals`
/// fallback then re-discovered the value and leaked it. The dict-step
/// resolver now distinguishes "private blocked" from "not found" and
/// the `PrivateBlocked` arm refuses to consult locals.
#[test]
fn private_field_does_not_leak_via_locals_fallback() {
    let result = eval_doc(
        r#"{
            #private
            secret: "shhh",
            alias: &sibling.secret,
            child: { leak: &root.secret }
        }"#,
    );
    assert!(
        matches!(&result, Err(RuntimeError::VariableNotFound(name, _)) if name.contains("secret")),
        "expected VariableNotFound for cross-dict private access, got {result:?}"
    );
}

/// Bug 3: AST-level path navigation through `Expr::List` only set
/// `path_node` on the per-element scope, never `list_context`, so
/// referenced elements that used `&index` would error with
/// "&index can only be used inside a list". The list step now mirrors
/// `eval.rs` by building per-element thunks and installing
/// `with_list_context` on the scope used to resolve into the chosen
/// element.
#[test]
fn ast_path_into_list_element_carries_list_context() {
    let result = eval_doc(
        r#"{
            list: [{ y: &index }],
            x: &sibling.list[0].y
        }"#,
    )
    .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(d.map.get("x"), Some(&Value::Int(0)));
}

/// Bug 4: paths containing a dynamic segment must not be cached, and
/// the dynamic expression must be evaluated exactly once. The previous
/// implementation evaluated dynamics up-front to mint a cache key, then
/// the actual lookup re-evaluated them — doubling host-side side
/// effects. We now bypass the cache entirely for dynamic paths and let
/// `eval_reference_path_from` perform the single evaluation.
#[test]
fn dynamic_path_segment_is_evaluated_only_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    CALLS.store(0, Ordering::SeqCst);

    struct CountingKey;
    impl crate::native_fn::RelonFunction for CountingKey {
        fn call(
            &self,
            _args: crate::native_fn::NativeArgs,
            _range: relon_parser::TokenRange,
        ) -> Result<Value, RuntimeError> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(Value::String("a".to_string()))
        }
    }

    let node = parse_doc(
        r#"{
            obj: { a: 1, b: 2 },
            v: &sibling.obj[counting_key()]
        }"#,
    );
    let mut ctx = Context::new().with_root(node);
    ctx.register_fn("counting_key", Arc::new(CountingKey));
    let result = Evaluator::new(Arc::new(ctx))
        .eval_root(&Arc::new(Scope::default()))
        .unwrap();
    let Value::Dict(d) = result else { panic!() };
    assert_eq!(d.map.get("v"), Some(&Value::Int(1)));
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "dynamic key expression should evaluate exactly once"
    );
}

#[test]
fn with_workspace_wires_entry_tree_into_analyzed_field() {
    use std::sync::Arc;
    // `Context::with_workspace` is a sugar that mounts the workspace
    // tree *and* surfaces the entry's per-file analyzed tree on the
    // legacy `analyzed` field, so single-file code paths that read
    // `Context::analyzed` keep working.
    let node = parse_doc("{ a: 1 }");
    let arc_node = Arc::new(node.clone());
    let mut ws = relon_analyzer::WorkspaceTree::new();
    ws.entry_id = "memory:entry".to_string();
    ws.modules.insert(
        "memory:entry".to_string(),
        Arc::new(relon_analyzer::analyze(&node)),
    );
    ws.nodes.insert("memory:entry".to_string(), arc_node);

    let ctx = Context::new().with_root(node).with_workspace(Arc::new(ws));
    assert!(
        ctx.analyzed.is_some(),
        "with_workspace should wire entry analyzed tree"
    );
    assert!(ctx.workspace.is_some());
}

#[test]
fn workspace_module_lookup_skips_reparse_during_evaluate_module_source() {
    // Set up a module the workspace pre-analyzed. The evaluator's
    // module-load path should pull both the parsed Node and the
    // AnalyzedTree out of the workspace rather than re-running the
    // parser. We verify by feeding the loader a *deliberately broken*
    // source string but a *valid* pre-parsed Node — if the evaluator
    // ever parses, it would fail; if it uses the workspace fast path,
    // it succeeds.
    use crate::module::{ModuleResolver, ModuleSource};
    use relon_parser::TokenRange;
    use std::sync::Arc;

    struct StubResolver;
    impl ModuleResolver for StubResolver {
        fn resolve(
            &self,
            path: &str,
            _scope: &Arc<Scope>,
            _range: TokenRange,
        ) -> Result<Option<ModuleSource>, RuntimeError> {
            if path != "stub/module" {
                return Ok(None);
            }
            Ok(Some(ModuleSource {
                canonical_id: "stub/module".to_string(),
                // Intentionally invalid Relon — only the workspace
                // fast path can succeed against this module.
                source: "<<<this would fail to parse>>>".to_string(),
                current_dir: String::new(),
            }))
        }
    }

    // Pre-parse a valid module body and stash it in the workspace.
    let module_node = parse_doc("{ greeting: \"hi\" }");
    let module_arc = Arc::new(module_node.clone());
    let mut ws = relon_analyzer::WorkspaceTree::new();
    ws.entry_id = "memory:entry".to_string();
    ws.modules.insert(
        "stub/module".to_string(),
        Arc::new(relon_analyzer::analyze(&module_node)),
    );
    ws.nodes
        .insert("stub/module".to_string(), Arc::clone(&module_arc));
    // Entry: imports the stub module by alias and reads `greeting`.
    let entry_node = parse_doc(
        r#"#import m from "stub/module"
        { msg: m.greeting }"#,
    );
    let entry_arc = Arc::new(entry_node.clone());
    ws.modules.insert(
        "memory:entry".to_string(),
        Arc::new(relon_analyzer::analyze(&entry_node)),
    );
    ws.nodes.insert("memory:entry".to_string(), entry_arc);

    let mut ctx = Context::new()
        .with_root(entry_node)
        .with_workspace(Arc::new(ws));
    ctx.prepend_module_resolver(Arc::new(StubResolver));
    let ctx = Arc::new(ctx);
    let result = Evaluator::new(Arc::clone(&ctx))
        .eval_root(&Arc::new(Scope::default()))
        .expect("workspace fast path should bypass the broken source string");
    let Value::Dict(d) = result else {
        panic!("expected dict, got {result:?}")
    };
    assert_eq!(d.map.get("msg"), Some(&Value::String("hi".to_string())));
}
