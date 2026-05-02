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
    use relon_parser::parse_base;
    use relon_parser::Span;

    #[test]
    fn test_user_defined_meta_logic() {
        let mut input = Span::new(
            r#"{
            "shout": @fn(v) v + "!!!",
            "multiply": @fn(a, b) a * b,
            "result_fn": multiply(10, 5),
            "result_dec": @shout "hello"
        }"#,
        );

        let node = parse_base(&mut input).expect("Parser failed");
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
        let mut input = Span::new(
            r#"{
            "invalid name": @fn(x) x + 1
        }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
    fn test_string_stdlib() {
        let mut input = Span::new(
            r#"{
            "words": split("rust,config,dsl", ","),
            "joined": join(&sibling.words, "-"),
            "replaced": replace("hello world", "world", "relon"),
            "upper": to_upper("Relon"),
            "lower": to_lower("Relon")
        }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
        } else {
            panic!("Expected Dict");
        }
    }

    #[test]
    fn test_dict_stdlib() {
        let mut input = Span::new(
            r#"{
            "base": { "a": 1, "b": 2 },
            "override": { "b": 3, "c": 4 },
            "merged": merge(&sibling.base, &sibling.override),
            "keys": keys(&sibling.merged),
            "values": values(&sibling.merged),
            "has_b": contains(&sibling.merged, "b"),
            "has_z": contains(&sibling.merged, "z")
        }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
        } else {
            panic!("Expected Dict");
        }
    }

    #[test]
    fn test_validation_custom_messages() {
        let mut input = Span::new(
            r#"{
            @min(1024, "port must be >= 1024")
            "port": 80
        }"#,
        );

        let node = parse_base(&mut input).unwrap();
        let ctx = Context::new().with_root(node.clone());
        let result = Evaluator::new(&ctx).eval(&node, &std::sync::Arc::new(Scope::default()));

        assert!(matches!(
            result,
            Err(RuntimeError::ValidationError(message, _)) if message == "port must be >= 1024"
        ));
    }

    #[test]
    fn test_cross_field_validation() {
        let mut input = Span::new(
            r#"@required_fields(["host", "port"])
            @requires("tls", "cert")
            @field_eq("password", "confirm")
            {
                "host": "localhost",
                "port": 8080,
                "tls": true,
                "cert": "cert.pem",
                "password": "secret",
                "confirm": "secret"
            }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
        let mut input = Span::new(
            r#"@requires("tls", "cert", "cert is required when tls is enabled")
            {
                "tls": true
            }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
        let mut input = Span::new(
            r#"{ 
            "base": { "x": 1 },
            "full": { ...&sibling.base, "y": 2 }
        }"#,
        );
        let node = parse_base(&mut input).unwrap();
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
        let mut input = Span::new(
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

        let node = parse_base(&mut input).unwrap();
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
    fn test_reference_resolution_caches_resolved_paths() {
        let mut input = Span::new(
            r#"{
            "a": 10 + 5,
            "b": &sibling.a,
            "c": &sibling.a
        }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
        let mut input = Span::new(
            r#"{
            "a": &sibling.b,
            "b": &sibling.a
        }"#,
        );

        let node = parse_base(&mut input).unwrap();
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
        // Relon document with references
        let mut input = Span::new(
            r#"{
            "a": 10,
            "b": &sibling.a + 5,
            "c": {
                "d": &uncle.b * 2
            }
        }"#,
        );

        let node = parse_base(&mut input).expect("Parser failed");
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
        let mut input = Span::new(r#"@import("tests_assets/a.relon") {}"#);
        let node = parse_base(&mut input).unwrap();
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
}
