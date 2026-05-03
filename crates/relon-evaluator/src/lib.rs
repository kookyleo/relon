pub mod error;
pub mod eval;
pub mod stdlib;
pub mod value;

pub use error::RuntimeError;
pub use eval::{Context, Evaluator, RelonFunction, Scope};
pub use value::Value;

#[cfg(test)]
mod tests {
    use super::*;
    use relon_parser::expr::parse_expr;
    use relon_parser::parse_document;
    use relon_parser::Span;

    fn parse_doc(source: &str) -> relon_parser::Node {
        parse_document(source).expect("Parser failed")
    }

    fn eval_doc(source: &str) -> Result<Value, RuntimeError> {
        let node = parse_doc(source);
        let ctx = Context::new().with_root(node.clone());
        Evaluator::new(&ctx).eval(&node, &std::sync::Arc::new(Scope::default()))
    }

    fn assert_number_type_mismatch(source: &str, found_type: &str) {
        let result = eval_doc(source);
        assert!(matches!(
            result,
            Err(RuntimeError::TypeMismatch { expected, found, .. })
                if expected == "Number" && found == found_type
        ));
    }

    #[test]
    fn test_user_defined_meta_logic() {
        let node = parse_doc(
            r#"{
            "shout": @fn(v) v + "!!!",
            "multiply": @fn(a, b) a * b,
            "result_fn": multiply(10, 5),
            "result_dec": @shout "hello"
        }"#,
        );
        let ctx = Context::new().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
        let scope = std::sync::Arc::new(Scope::default());

        let result = eval.eval(&node, &scope).expect("Evaluation failed");

        if let Value::Dict(map) = result {
            assert_eq!(map.get("result_fn").unwrap(), &Value::Int(50));
            assert_eq!(
                map.get("result_dec").unwrap(),
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
            "invalid name": @fn(x) x + 1
        }"#,
        );

        let ctx = Context::new().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
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
        let result = Evaluator::new(&ctx)
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
                map.get("add").unwrap(),
                &Value::Float(ordered_float::OrderedFloat(3.5))
            );
            assert_eq!(
                map.get("sub").unwrap(),
                &Value::Float(ordered_float::OrderedFloat(2.5))
            );
            assert_eq!(
                map.get("mul").unwrap(),
                &Value::Float(ordered_float::OrderedFloat(3.0))
            );
            assert_eq!(
                map.get("div").unwrap(),
                &Value::Float(ordered_float::OrderedFloat(2.5))
            );
            assert_eq!(
                map.get("mod").unwrap(),
                &Value::Float(ordered_float::OrderedFloat(1.0))
            );
            assert_eq!(map.get("lt").unwrap(), &Value::Bool(true));
            assert_eq!(map.get("ge").unwrap(), &Value::Bool(true));
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
            assert_eq!(map.get("port").unwrap(), &Value::Int(3));
            assert!(matches!(map.get("config").unwrap(), Value::Dict(_)));
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
        let node = parse_doc(
            r#"{
            "words": string.split("rust,config,dsl", ","),
            "joined": string.join(&sibling.words, "-"),
            "replaced": string.replace("hello world", "world", "relon"),
            "upper": string.upper("Relon"),
            "lower": string.lower("Relon"),
            "has_config": string.contains("rust config dsl", "config")
        }"#,
        );

        let ctx = Context::new().with_root(node.clone());
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();

        if let Value::Dict(map) = result {
            assert_eq!(
                map.get("joined").unwrap(),
                &Value::String("rust-config-dsl".to_string())
            );
            assert_eq!(
                map.get("replaced").unwrap(),
                &Value::String("hello relon".to_string())
            );
            assert_eq!(
                map.get("upper").unwrap(),
                &Value::String("RELON".to_string())
            );
            assert_eq!(
                map.get("lower").unwrap(),
                &Value::String("relon".to_string())
            );
            assert_eq!(map.get("has_config").unwrap(), &Value::Bool(true));
        } else {
            panic!("Expected Dict");
        }
    }

    #[test]
    fn test_dict_stdlib() {
        let node = parse_doc(
            r#"{
            "base": { "a": 1, "b": 2 },
            "override": { "b": 3, "c": 4 },
            "merged": dict.merge(&sibling.base, &sibling.override),
            "keys": dict.keys(&sibling.merged),
            "values": dict.values(&sibling.merged),
            "has_b": dict.has_key(&sibling.merged, "b"),
            "has_z": dict.has_key(&sibling.merged, "z"),
            "list_has_b": list.contains(&sibling.keys, "b")
        }"#,
        );

        let ctx = Context::new().with_root(node.clone());
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();

        if let Value::Dict(map) = result {
            assert_eq!(
                map.get("keys").unwrap(),
                &Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ])
            );
            assert_eq!(
                map.get("values").unwrap(),
                &Value::List(vec![Value::Int(1), Value::Int(3), Value::Int(4)])
            );
            assert_eq!(map.get("has_b").unwrap(), &Value::Bool(true));
            assert_eq!(map.get("has_z").unwrap(), &Value::Bool(false));
            assert_eq!(map.get("list_has_b").unwrap(), &Value::Bool(true));
        } else {
            panic!("Expected Dict");
        }
    }

    #[test]
    fn test_virtual_stdlib_modules() {
        let result = eval_doc(
            r#"@import("std/list", as="list")
            @import("std/math", as="math")
            @import("std/value", as="value")
            @import("std/is", as="is")
            @import("std/string", as="string")
            @import("std/dict", as="dict")
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
            assert_eq!(map.get("first").unwrap(), &Value::Int(10));
            assert_eq!(
                map.get("compact").unwrap(),
                &Value::List(vec![Value::Int(1), Value::Int(2)])
            );
            assert_eq!(map.get("clamped").unwrap(), &Value::Int(10));
            assert_eq!(
                map.get("defaulted").unwrap(),
                &Value::String("fallback".to_string())
            );
            assert_eq!(map.get("kept_false").unwrap(), &Value::Bool(false));
            assert_eq!(map.get("is_number").unwrap(), &Value::Bool(true));
            assert_eq!(map.get("is_empty").unwrap(), &Value::Bool(true));
            assert_eq!(
                map.get("joined").unwrap(),
                &Value::String("a-b".to_string())
            );
            assert_eq!(map.get("has_key").unwrap(), &Value::Bool(true));
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
        let result = Evaluator::new(&ctx).eval(&node, &std::sync::Arc::new(Scope::default()));

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
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();

        if let Value::Dict(map) = result {
            assert_eq!(
                map.get("host").unwrap(),
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
        let result = Evaluator::new(&ctx).eval(&node, &std::sync::Arc::new(Scope::default()));

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
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();
        if let Value::Dict(map) = result {
            let full = map.get("full").unwrap();
            if let Value::Dict(inner) = full {
                assert_eq!(inner.get("x").unwrap(), &Value::Int(1));
                assert_eq!(inner.get("y").unwrap(), &Value::Int(2));
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
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();

        if let Value::Dict(map) = result {
            assert_eq!(map.get("port_copy").unwrap(), &Value::Int(8080));
            assert_eq!(map.get("late_spread_copy").unwrap(), &Value::Int(8080));
            assert_eq!(map.get("late_key_copy").unwrap(), &Value::Int(3000));
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
            assert_eq!(map.get("a").unwrap(), &Value::Int(2));
            assert_eq!(map.get("b").unwrap(), &Value::Int(2));
        } else {
            panic!("Expected Dict");
        }
    }

    #[test]
    fn test_closure_body_references_use_closure_body_root() {
        let result = eval_doc(
            r#"{
            @fn(x)
            "make": {
                "b": &sibling.a,
                "a": x
            },
            "one": make(1)
        }"#,
        )
        .unwrap();

        if let Value::Dict(map) = result {
            let Value::Dict(one) = map.get("one").unwrap() else {
                panic!("Expected Dict");
            };
            assert_eq!(one.get("a").unwrap(), &Value::Int(1));
            assert_eq!(one.get("b").unwrap(), &Value::Int(1));
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
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();

        if let Value::Dict(map) = result {
            assert_eq!(map.get("b").unwrap(), &Value::Int(15));
            assert_eq!(map.get("c").unwrap(), &Value::Int(15));
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
        let result = Evaluator::new(&ctx).eval(&node, &std::sync::Arc::new(Scope::default()));

        assert!(matches!(result, Err(RuntimeError::CircularReference(_))));
    }

    #[test]
    fn test_list_comprehension() {
        let mut input = Span::new(r#"[x * 2 for x in range(5) if x % 2 == 0]"#);
        let node = parse_expr(&mut input).unwrap();
        let ctx = Context::new();
        let result = Evaluator::new(&ctx)
            .eval(&node, &std::sync::Arc::new(Scope::default()))
            .unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::Int(0), Value::Int(4), Value::Int(8)])
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
        let eval = Evaluator::new(&ctx);
        let scope = std::sync::Arc::new(Scope::default());

        let result = eval.eval(&node, &scope).expect("Evaluation failed");

        if let Value::Dict(map) = result {
            assert_eq!(map.get("a").unwrap(), &Value::Int(10));
            assert_eq!(map.get("b").unwrap(), &Value::Int(15)); // 10 + 5

            if let Value::Dict(inner) = map.get("c").unwrap() {
                assert_eq!(inner.get("d").unwrap(), &Value::Int(30)); // 15 * 2
            } else {
                panic!()
            }
        } else {
            panic!()
        }
    }

    #[test]
    fn test_circular_import() {
        let node = parse_doc(r#"@import("tests_assets/a.relon") {}"#);
        let ctx = Context::new().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
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
        let dir =
            std::env::temp_dir().join(format!("relon-canonical-import-{}", std::process::id()));
        let subdir = dir.join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(dir.join("lib.relon"), r#"{ value: 1 }"#).unwrap();

        let node = parse_doc(
            r#"@import("sub/../lib.relon", as="a")
            @import("lib.relon", as="b")
            {}"#,
        );
        let ctx = Context::new().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
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
            r#"@import("lib.relon", as="lib")
            {
                "b": lib.b
            }"#,
        );
        let ctx = Context::new().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
        let scope = std::sync::Arc::new(Scope {
            current_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        });

        let result = eval.eval(&node, &scope).unwrap();

        if let Value::Dict(map) = result {
            assert_eq!(map.get("b").unwrap(), &Value::Int(1));
        } else {
            panic!("Expected Dict");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_loading_modules_restored_after_module_parse_error() {
        let dir =
            std::env::temp_dir().join(format!("relon-import-parse-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.relon"), "{} trailing").unwrap();

        let node = parse_doc(r#"@import("bad.relon") {}"#);
        let ctx = Context::new().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
        let scope = std::sync::Arc::new(Scope {
            current_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        });

        let result = eval.eval(&node, &scope);

        assert!(matches!(result, Err(RuntimeError::ModuleParseError { .. })));
        assert!(ctx.loading_modules.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
