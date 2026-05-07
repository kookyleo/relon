pub mod arithmetic;
pub mod builtin_decorators;
pub mod decorator;
pub mod decorator_names;
pub mod error;
pub mod eval;
pub mod module;
pub mod native_fn;
pub mod reference;
pub mod schema;
pub mod scope;
pub mod stdlib;
pub mod value;

pub use decorator::{DecoratorPlugin, PreEvalOutcome};
pub use error::RuntimeError;
pub use eval::{Capabilities, Context, Evaluator, NativeFnGate};
pub use module::{FilesystemModuleResolver, ModuleResolver, ModuleSource, StdModuleResolver};
pub use native_fn::NativeFnCaps;
pub use native_fn::{EvaluatedArg, NativeArgs, RelonFunction};
pub use scope::{ListContext, Scope, Thunk};
pub use value::{SchemaField, Value, ValueDict};

#[cfg(test)]
mod tests {
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
        Evaluator::new(&ctx).eval_root(&std::sync::Arc::new(Scope::default()))
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
        let eval = Evaluator::new(&ctx);
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
            r#"@import("std/string", as="string")
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
            r#"@import("std/dict", as="dict")
            @import("std/list", as="list")
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
        let result = Evaluator::new(&ctx)
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
        let result = Evaluator::new(&ctx)
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
        let result = Evaluator::new(&ctx).eval(&node, &std::sync::Arc::new(Scope::default()));

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
        let result = Evaluator::new(&ctx)
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
        let eval = Evaluator::new(&ctx);
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
        let node = parse_doc(r#"@import("tests_assets/a.relon") {}"#);
        let ctx = fully_granted_ctx().with_root(node.clone());
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
        let ctx = fully_granted_ctx().with_root(node.clone());
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
        let ctx = fully_granted_ctx().with_root(node.clone());
        let eval = Evaluator::new(&ctx);
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
        let dir =
            std::env::temp_dir().join(format!("relon-import-parse-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.relon"), "{} trailing").unwrap();

        let node = parse_doc(r#"@import("bad.relon") {}"#);
        let ctx = fully_granted_ctx().with_root(node.clone());
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
            @schema
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
            @schema
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
            @schema
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
            @schema
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
            @schema
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
            @schema
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
            @schema User: { 
                String name: *,
                @expect("Age must be positive")
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
            @schema Base: { String type: * },
            @schema Button: &sibling.Base + { String label: * },

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
            @schema Base: { @default("info") String level: * },
            @schema Error: &sibling.Base + { level: "error" },

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
            @schema Base: { String name: * },
            @schema User: &sibling.Base + { Int age: * },

            User alice: { name: "Alice", age: 30 }
        }"#,
        )
        .unwrap();
        assert!(matches!(ok, Value::Dict(_)));

        let missing = eval_doc(
            r#"{
            @schema Base: { String name: * },
            @schema User: &sibling.Base + { Int age: * },

            User alice: { name: "Alice" }
        }"#,
        );
        assert!(matches!(missing, Err(RuntimeError::TypeMismatch { .. })));

        let wrong_type = eval_doc(
            r#"{
            @schema Base: { String name: * },
            @schema User: &sibling.Base + { Int age: * },

            User alice: { name: "Alice", age: "thirty" }
        }"#,
        );
        assert!(matches!(wrong_type, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn test_deep_merge() {
        let result = eval_doc(
            r#"@import("std/dict", as="dict")
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
            @schema Base: { Int port: (p) => p > 0 },
            @schema Derived: &sibling.Base + { Int port: (p) => p < 100 },

            Derived in_range: { port: 50 }
        }"#,
        )
        .unwrap();
        assert!(matches!(ok, Value::Dict(_)));

        // Violates the Derived constraint (port < 100): must fail because
        // composition is now AND, not "right side wins".
        let too_high = eval_doc(
            r#"{
            @schema Base: { Int port: (p) => p > 0 },
            @schema Derived: &sibling.Base + { Int port: (p) => p < 100 },

            Derived too_high: { port: 200 }
        }"#,
        );
        assert!(matches!(too_high, Err(RuntimeError::TypeMismatch { .. })));

        // Violates the Base constraint (port > 0): must still fail under
        // composition — the right-hand `< 100` predicate doesn't shadow it.
        let negative = eval_doc(
            r#"{
            @schema Base: { Int port: (p) => p > 0 },
            @schema Derived: &sibling.Base + { Int port: (p) => p < 100 },

            Derived negative: { port: -5 }
        }"#,
        );
        assert!(matches!(negative, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn test_recursive_schema() {
        let result = eval_doc(
            r#"{
            @schema
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
            @schema
            Server: {
                @expect("Port must be between 0 and 65535")
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
            @schema Image: { name: String, url: String },
            @schema Text: { name: String, content: String },
            
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
            @schema Button: { String label: * },
            @schema Link:   { String label: * },
            
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
        // `@brand(Weather)` at field-value position is the decorator-form
        // analogue of `Weather w: {...}`: it runs `check_type` against the
        // schema and writes `dict.brand`. Verify both effects on a value
        // that satisfies the schema.
        let result = eval_doc(
            r#"{
            @schema Weather: { String location: *, Int temperature: * },

            w: @brand(Weather) {
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
        // `@brand` must reach the same outcome through the same `check_type`
        // call.
        let result = eval_doc(
            r#"{
            @schema Weather: { String location: *, Int temperature: * },

            w: @brand(Weather) {
                location: "Shanghai",
                temperature: "hot"
            }
        }"#,
        );
        assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn test_brand_decorator_string_form_equivalent() {
        // `@brand("Weather")` and `@brand(Weather)` resolve to the same
        // type name; the brand written into the dict must be identical.
        let result = eval_doc(
            r#"{
            @schema Weather: { String location: *, Int temperature: * },

            w: @brand("Weather") {
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
        // The whole point of `@brand` is reaching positions where a
        // `Type field:` hint can't be written — the document root being
        // the canonical example. The schema lives in an outer scope
        // injected by the host (the parser doesn't allow a root-node
        // type hint, so this is the only way to brand a root dict).
        use std::collections::HashMap;
        let node = parse_doc(
            r#"@brand(Weather) {
            location: "Berlin",
            temperature: 15
        }"#,
        );
        let ctx = Context::new().with_root(node);
        // Synthesize a `Weather` schema and seed it into the surrounding
        // scope so the root-level `@brand` can resolve it.
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

        let result = Evaluator::new(&ctx).eval_root(&outer_scope).unwrap();
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
    fn test_brand_decorator_stacks_with_import_spread() {
        // `@import(spread=true)` injects locals (here: a schema) into the
        // surrounding scope; `@brand` then validates the dict against that
        // newly-visible schema. Order matters — `@import`'s `pre_eval`
        // runs first so the schema is in scope by the time `@brand`'s
        // wrap pass calls `check_type`.
        let dir = std::env::temp_dir().join(format!(
            "relon-brand-import-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lib_path = dir.join("weather_schema.relon");
        std::fs::write(
            &lib_path,
            r#"{
                @schema Weather: { String location: *, Int temperature: * }
            }"#,
        )
        .unwrap();

        let src = format!(
            r#"{{
                w: @import("{}", spread=true) @brand(Weather) {{
                    location: "Paris",
                    temperature: 20
                }}
            }}"#,
            lib_path.to_string_lossy()
        );

        let node = parse_doc(&src);
        let ctx = fully_granted_ctx().with_root(node);
        let result = Evaluator::new(&ctx).eval_root(&std::sync::Arc::new(Scope::default()));
        let _ = std::fs::remove_dir_all(&dir);
        let result = result.unwrap();

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
        // `Weather w: @brand(Weather) {...}` expresses the same intent
        // twice. Refuse the combination so the user picks one form.
        let result = eval_doc(
            r#"{
            @schema Weather: { String location: *, Int temperature: * },

            Weather w: @brand(Weather) {
                location: "Shanghai",
                temperature: 22
            }
        }"#,
        );
        assert!(matches!(
            result,
            Err(RuntimeError::UnsupportedOperator(msg, _)) if msg.contains("@brand")
        ));
    }

    #[test]
    fn test_brand_decorator_unknown_schema_matches_field_form() {
        // The field-form `NotDeclared x: {...}` produces `VariableNotFound`
        // when `NotDeclared` isn't in scope (see `check_custom_schema`).
        // `@brand(NotDeclared) {...}` must produce the same error so the
        // two entry points stay observationally identical.
        let result = eval_doc(
            r#"{
            w: @brand(NotDeclared) { a: 1 }
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

    // -------- Task A: generic / optional types in `@brand(...)` --------

    #[test]
    fn test_brand_decorator_generic_dict_validates_and_brands() {
        // `@brand(Dict<String, Int>)` runs `check_dict` against the value
        // (every entry's value must be `Int`) and stamps the dict with a
        // brand string that round-trips through type matches and JSON.
        // `Dict` is the canonical builtin — `Map<...>` would fall through
        // to `check_custom_schema` and fail with `VariableNotFound("Map")`
        // unless the host registers a `Map` alias schema.
        let result = eval_doc(
            r#"{
            counters: @brand(Dict<String, Int>) {
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
            counters: @brand(Dict<String, Int>) {
                hits: 1,
                misses: "lots"
            }
        }"#,
        );
        assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn test_brand_decorator_generic_single_param_brand_string() {
        // `@brand(Foo<T>)` with no host-supplied `Foo` schema falls back to
        // `check_custom_schema` — the lookup fails on `Foo`, mirroring the
        // field form. Brand serialization for the generic shape, however,
        // is still well-defined: verify it through the lookup error path
        // by introducing `Foo` as an alias to a permissive schema first.
        let result = eval_doc(
            r#"{
            @schema Foo: { Any value: * },

            wrapped: @brand(Foo<T>) {
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
            decorated: @brand(Dict<String, Int>) { a: 1, b: 2 }
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
        // `@brand(Weather?)` — the `?` suffix flows into the brand string
        // so type guards see the optionality marker. The schema validation
        // proceeds against the underlying `Weather` schema.
        let result = eval_doc(
            r#"{
            @schema Weather: { String location: *, Int temperature: * },

            w: @brand(Weather?) {
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

    // -------- Task B: `@brand(X)` at schema-field position --------

    #[test]
    fn test_brand_decorator_in_schema_field_equivalent_to_type_prefix() {
        // `@schema S: { @brand(String) name: * }` is the decorator-form
        // mirror of `@schema S: { String name: * }`. A non-`String` value
        // on the schema instance must reject with `TypeMismatch`, and a
        // `String` value must validate cleanly.
        let ok = eval_doc(
            r#"{
            @schema S: { @brand(String) name: * },

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
            @schema S: { @brand(String) name: * },

            S inst: { name: 42 }
        }"#,
        );
        assert!(matches!(bad, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn test_brand_decorator_in_schema_field_dotted_path() {
        // `@brand(geo.Location) loc: *` — dotted-path arg form. The schema
        // must validate against the resolved `geo.Location` binding (here
        // re-bound in the same dict so static lookup works). Same as the
        // type-prefix form `geo.Location loc: *` — neither auto-brands
        // the nested instance; the user is expected to apply `@brand` at
        // the instance position when an explicit brand is desired.
        let result = eval_doc(
            r#"{
            geo: { Location: @schema { Number lat: *, Number lon: * } },
            @schema Place: {
                @brand(geo.Location) loc: *,
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
        // satisfy `geo.Location` — proving the `@brand`-derived type hint
        // really drove `check_type`.
        let bad = eval_doc(
            r#"{
            geo: { Location: @schema { Number lat: *, Number lon: * } },
            @schema Place: {
                @brand(geo.Location) loc: *,
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
        // `@brand(Dict<String, Int>) m: *` lifts the generic type into the
        // field's type hint; instances with non-Int values must reject.
        let ok = eval_doc(
            r#"{
            @schema Counters: {
                @brand(Dict<String, Int>) m: *
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
            @schema Counters: {
                @brand(Dict<String, Int>) m: *
            },

            Counters c: { m: { a: 1, b: "two" } }
        }"#,
        );
        assert!(matches!(bad, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn test_brand_decorator_in_schema_field_conflict_with_explicit_type() {
        // When a schema field carries BOTH an explicit type prefix AND a
        // `@brand(...)` decorator, the analyzer flags it as a conflict.
        // Source: `@brand(Bar) Foo x: *` — both forms try to declare `x`'s
        // type. Verify the diagnostic shape directly via `relon_analyzer`.
        let node = parse_doc(
            r#"{
            @schema S: { @brand(Bar) Foo x: * }
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
        // `@brand(X) y: *` is a complete field declaration — the schema
        // pass must NOT emit `SchemaFieldUntyped` for it.
        let node = parse_doc(
            r#"{
            @schema S: { @brand(String) name: * }
        }"#,
        );
        let tree = relon_analyzer::analyze(&node);
        assert!(
            !tree
                .diagnostics
                .iter()
                .any(|d| matches!(d, relon_analyzer::Diagnostic::SchemaFieldUntyped { .. })),
            "should not emit SchemaFieldUntyped for `@brand`-typed field, got {:?}",
            tree.diagnostics
        );
    }

    #[test]
    fn test_brand_decorator_in_schema_field_end_to_end_with_meta() {
        // End-to-end: a `@brand(...)`-typed field still composes with the
        // other schema-field meta decorators (`@expect`, `@default`).
        // The custom error must surface on a missing field; the default
        // must populate a missing field in the instance.
        let result = eval_doc(
            r#"{
            @schema User: {
                @brand(String) name: *,
                @default(0) @brand(Int) age: *
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
            @schema User: {
                @default("guest")
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
        // A closure passed to @default fires at validation time with the
        // partially-populated instance bound to `self`, computing the field
        // from sibling values.
        let result = eval_doc(
            r#"{
            @schema User: {
                String first: *,
                String last: *,
                @default((self) => self.first + " " + self.last)
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
            @schema User: {
                String first: *,
                String last: *,
                @default((self) => self.first + " " + self.last)
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
        let eval = Evaluator::new(&ctx);
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
                @schema Notification: Enum<
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
                @schema Notification: Enum<
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
                @schema N: Enum<A { x: Int }>,
                msg: N.Bogus { x: 1 }
            }"#,
        );
        assert!(matches!(result, Err(RuntimeError::TypeMismatch { .. })));
    }

    #[test]
    fn variant_ctor_missing_required_field_errors() {
        let result = eval_doc(
            r#"{
                @schema N: Enum<Email { address: String, subject: String }>,
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
                @schema N: Enum<Email { address: String }>,
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
                @schema N: Enum<
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
                @schema Status: { String mode: Enum<"up", "down"> },
                Status s: { mode: "up" }
            }"#,
        );
        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn untagged_enum_type_set_still_validates() {
        let result = eval_doc(
            r#"{
                @schema Theme: { id: Enum<Int, String> },
                Theme t: { id: 7 }
            }"#,
        );
        assert!(result.is_ok(), "{:?}", result);
    }
}

#[cfg(test)]
mod sandbox_tests {
    //! Capability / sandbox layer (Capabilities + FilesystemModuleResolver
    //! root + step counter + value-size watermark + register_fn_with_caps).
    //!
    //! Each test pins one knob; together they pin down the spec from
    //! `tmp/critical-analysis-round2.md`.

    use super::*;
    use std::sync::Arc;

    fn parse(source: &str) -> relon_parser::Node {
        relon_parser::parse_document(source).expect("parse")
    }

    fn eval_with(ctx: Context, source: &str) -> Result<Value, RuntimeError> {
        let node = parse(source);
        let ctx = ctx.with_root(node);
        Evaluator::new(&ctx).eval_root(&Arc::new(Scope::default()))
    }

    #[test]
    fn sandboxed_context_rejects_default_filesystem_imports() {
        // Sandboxed `Context` ships a default-rejecting filesystem
        // resolver, so any non-`std/` import must fail with
        // `CapabilityDenied` before the OS is touched.
        let dir = std::env::temp_dir().join(format!("relon-sbox-default-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.relon"), r#"{ secret: "leak" }"#).unwrap();

        let node = parse(r#"@import("lib.relon", as="lib") { x: lib.secret }"#);
        let ctx = Context::sandboxed().with_root(node);
        let scope = Arc::new(Scope {
            current_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        });
        let result = Evaluator::new(&ctx).eval_root(&scope);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(&result, Err(RuntimeError::CapabilityDenied { name, .. }) if name.contains("@import")),
            "expected CapabilityDenied, got {result:?}"
        );
    }

    #[test]
    fn filesystem_resolver_with_root_dir_allows_paths_under_root() {
        let dir = std::env::temp_dir().join(format!("relon-sbox-root-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.relon"), r#"{ value: 42 }"#).unwrap();

        let mut ctx = Context::sandboxed();
        // Replace the default-rejecting resolver with one rooted at `dir`.
        ctx.module_resolvers = vec![
            Arc::new(StdModuleResolver),
            Arc::new(FilesystemModuleResolver::with_root_dir(&dir)),
        ];
        let node = parse(r#"@import("lib.relon", as="lib") { v: lib.value }"#);
        let ctx = ctx.with_root(node);
        let scope = Arc::new(Scope {
            current_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        });

        let result = Evaluator::new(&ctx).eval_root(&scope).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let Value::Dict(d) = result else {
            panic!("expected dict");
        };
        assert_eq!(d.map.get("v").unwrap(), &Value::Int(42));
    }

    #[test]
    fn filesystem_resolver_rejects_traversal_outside_root() {
        // `../escape.relon` resolves outside the configured root after
        // canonicalization → `CapabilityDenied`, not `IoError`.
        let outer = std::env::temp_dir().join(format!("relon-sbox-out-{}", std::process::id()));
        let inner = outer.join("inside");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(outer.join("escape.relon"), r#"{ leak: 1 }"#).unwrap();

        let mut ctx = Context::sandboxed();
        ctx.module_resolvers = vec![
            Arc::new(StdModuleResolver),
            Arc::new(FilesystemModuleResolver::with_root_dir(&inner)),
        ];
        let node = parse(r#"@import("../escape.relon", as="x") { y: x.leak }"#);
        let ctx = ctx.with_root(node);
        let scope = Arc::new(Scope {
            current_dir: inner.to_string_lossy().to_string(),
            ..Default::default()
        });
        let result = Evaluator::new(&ctx).eval_root(&scope);
        let _ = std::fs::remove_dir_all(&outer);

        assert!(
            matches!(&result, Err(RuntimeError::CapabilityDenied { reason, .. }) if reason.contains("escapes")),
            "expected CapabilityDenied, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_resolver_rejects_symlink_escape() {
        // A symlink inside the root that points outside it must be rejected
        // (canonicalization resolves symlinks before the prefix check).
        let outer = std::env::temp_dir().join(format!("relon-sbox-sym-{}", std::process::id()));
        let inner = outer.join("inside");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(outer.join("target.relon"), r#"{ leak: 1 }"#).unwrap();
        let link = inner.join("link.relon");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(outer.join("target.relon"), &link).unwrap();

        let mut ctx = Context::sandboxed();
        ctx.module_resolvers = vec![
            Arc::new(StdModuleResolver),
            Arc::new(FilesystemModuleResolver::with_root_dir(&inner)),
        ];
        let node = parse(r#"@import("link.relon", as="x") { y: x.leak }"#);
        let ctx = ctx.with_root(node);
        let scope = Arc::new(Scope {
            current_dir: inner.to_string_lossy().to_string(),
            ..Default::default()
        });
        let result = Evaluator::new(&ctx).eval_root(&scope);
        let _ = std::fs::remove_dir_all(&outer);

        assert!(
            matches!(&result, Err(RuntimeError::CapabilityDenied { reason, .. }) if reason.contains("escapes")),
            "expected CapabilityDenied for symlink escape, got {result:?}"
        );
    }

    #[test]
    fn max_steps_aborts_runaway_recursion() {
        // Spawn on a thread with a deliberately generous stack so the
        // step-budget gate has room to fire before debug-build frames
        // exhaust the platform default (~512KB on macOS test threads).
        let handle = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let mut ctx = Context::sandboxed();
                ctx.capabilities.max_steps = Some(100);
                eval_with(ctx, r#"{ loop(): loop(), "go": loop() }"#)
            })
            .unwrap();
        let result = handle.join().unwrap();
        assert!(
            matches!(
                result,
                Err(RuntimeError::StepLimitExceeded { limit: 100, .. })
            ),
            "expected StepLimitExceeded, got {result:?}"
        );
    }

    #[test]
    fn max_steps_does_not_fire_under_limit() {
        // Sanity check: a small program well under the budget completes
        // normally — proves the counter isn't hair-trigger.
        let mut ctx = Context::sandboxed();
        ctx.capabilities.max_steps = Some(10_000);
        let result = eval_with(ctx, r#"{ a: 1, b: 2, c: a + b }"#).unwrap();
        let Value::Dict(d) = result else {
            panic!("expected dict")
        };
        assert_eq!(d.map.get("c").unwrap(), &Value::Int(3));
    }

    #[test]
    fn max_value_bytes_rejects_oversized_list() {
        // The watermark fires at evaluator-side construction sites
        // (literal lists, dict-merge, list-comprehension). Stdlib-built
        // values like `range(...)` aren't gated — by design, since the
        // host owns those caps. Cover the literal-list path here.
        let mut ctx = Context::sandboxed();
        ctx.capabilities.max_value_bytes = Some(3);
        let result = eval_with(ctx, r#"{ "big": [1, 2, 3, 4, 5] }"#);
        assert!(
            matches!(
                result,
                Err(RuntimeError::ValueTooLarge {
                    limit: 3,
                    actual: 5,
                    ..
                })
            ),
            "expected ValueTooLarge, got {result:?}"
        );
    }

    #[test]
    fn max_value_bytes_rejects_oversized_dict() {
        let mut ctx = Context::sandboxed();
        ctx.capabilities.max_value_bytes = Some(2);
        let result = eval_with(ctx, r#"{ a: 1, b: 2, c: 3, d: 4 }"#);
        assert!(
            matches!(
                result,
                Err(RuntimeError::ValueTooLarge {
                    limit: 2,
                    actual: 4,
                    ..
                })
            ),
            "expected ValueTooLarge, got {result:?}"
        );
    }

    #[test]
    fn legacy_register_fn_callable_under_sandbox() {
        // `register_fn` (no caps) is treated as fully trusted — a
        // sandboxed Context with an empty `allow_native_fn` set must
        // still let it through.
        struct Echo;
        impl crate::native_fn::RelonFunction for Echo {
            fn call(
                &self,
                args: crate::native_fn::NativeArgs,
                _range: relon_parser::TokenRange,
            ) -> Result<Value, RuntimeError> {
                Ok(args.get(0).cloned().unwrap_or(Value::Null))
            }
        }

        let mut ctx = Context::sandboxed();
        ctx.register_fn("echo", Arc::new(Echo));
        let result = eval_with(ctx, r#"{ "x": echo(7) }"#).unwrap();
        let Value::Dict(d) = result else {
            panic!("expected dict")
        };
        assert_eq!(d.map.get("x").unwrap(), &Value::Int(7));
    }

    #[test]
    fn register_fn_with_caps_rejected_in_sandbox_without_allowlist() {
        struct ReadFs;
        impl crate::native_fn::RelonFunction for ReadFs {
            fn call(
                &self,
                _args: crate::native_fn::NativeArgs,
                _range: relon_parser::TokenRange,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::String("contents".to_string()))
            }
        }

        let mut ctx = Context::sandboxed();
        ctx.register_fn_with_caps(
            "fs.read",
            NativeFnGate { reads_fs: true },
            Arc::new(ReadFs),
        );
        let result = eval_with(ctx, r#"{ "data": fs.read() }"#);
        assert!(
            matches!(&result, Err(RuntimeError::CapabilityDenied { name, .. }) if name == "fs.read"),
            "expected CapabilityDenied, got {result:?}"
        );
    }

    #[test]
    fn register_fn_with_caps_permitted_when_in_allowlist() {
        struct ReadFs;
        impl crate::native_fn::RelonFunction for ReadFs {
            fn call(
                &self,
                _args: crate::native_fn::NativeArgs,
                _range: relon_parser::TokenRange,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::String("contents".to_string()))
            }
        }

        let mut ctx = Context::sandboxed();
        ctx.capabilities
            .allow_native_fn
            .insert("fs.read".to_string());
        ctx.register_fn_with_caps(
            "fs.read",
            NativeFnGate { reads_fs: true },
            Arc::new(ReadFs),
        );
        let result = eval_with(ctx, r#"{ "data": fs.read() }"#).unwrap();
        let Value::Dict(d) = result else {
            panic!("expected dict")
        };
        assert_eq!(
            d.map.get("data").unwrap(),
            &Value::String("contents".to_string())
        );
    }

    #[test]
    fn fully_granted_caps_let_gated_fns_through() {
        // `Capabilities::all_granted()` flips `allow_all_native_fn`,
        // so even `register_fn_with_caps`-registered fns go through
        // without an explicit allowlist entry.
        struct ReadFs;
        impl crate::native_fn::RelonFunction for ReadFs {
            fn call(
                &self,
                _args: crate::native_fn::NativeArgs,
                _range: relon_parser::TokenRange,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::Int(1))
            }
        }

        let mut ctx = Context::sandboxed();
        ctx.capabilities = Capabilities::all_granted();
        ctx.register_fn_with_caps(
            "fs.read",
            NativeFnGate { reads_fs: true },
            Arc::new(ReadFs),
        );
        let result = eval_with(ctx, r#"{ "n": fs.read() }"#).unwrap();
        let Value::Dict(d) = result else {
            panic!("expected dict")
        };
        assert_eq!(d.map.get("n").unwrap(), &Value::Int(1));
    }

    #[test]
    fn std_module_resolver_works_under_full_sandbox() {
        // `std/...` modules are virtual + zero-IO, so they must keep
        // working even under the strictest sandbox (no fs root, no
        // native-fn allowlist, etc.).
        let result = eval_with(
            Context::sandboxed(),
            r#"@import("std/list", as="list")
            { "first": list.first([10, 20, 30]) }"#,
        )
        .unwrap();
        let Value::Dict(d) = result else {
            panic!("expected dict")
        };
        assert_eq!(d.map.get("first").unwrap(), &Value::Int(10));
    }

    #[test]
    fn test_parameterized_schema() {
        let src = r#"{
            @schema Page<T>: {
                List<T> items: *
            },
            Page<Int> ok_page: {
                items: [1, 2, 3]
            },
            // This should fail validation because the item is a String
            Page<String> bad_page: {
                items: [1, 2, 3]
            }
        }"#;

        let node = relon_parser::parse_document(src).unwrap();
        let analyzed = relon_analyzer::analyze(&node);
        let ctx = Context::default()
            .with_root(node)
            .with_analyzed(Arc::new(analyzed));
        let evaluator = Evaluator::new(&ctx);
        let err = evaluator
            .eval_root(&Arc::new(Scope::default()))
            .unwrap_err();
        assert!(matches!(err, RuntimeError::TypeMismatch { .. }));
        if let RuntimeError::TypeMismatch {
            expected, found, ..
        } = err
        {
            assert_eq!(expected, "String");
            assert_eq!(found, "Int");
        }
    }

    #[test]
    fn test_brand_registry() {
        let mut ctx = Context::default();
        // Register a schema 'Email' globally
        let email_schema = Value::Schema {
            generics: Vec::new(),
            fields: {
                let mut fields = std::collections::HashMap::new();
                fields.insert(
                    "address".to_string(),
                    crate::value::SchemaField {
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
                fields
            },
        };
        ctx.register_schema("Email", email_schema);

        // Usage site doesn't define 'Email', but uses it via @brand
        let src = r#"{
            @brand(Email)
            "me": { "address": "test@example.com" }
        }"#;

        let result = eval_with(ctx, src).unwrap();
        let Value::Dict(d) = result else { panic!() };
        let me = d.map.get("me").unwrap();
        if let Value::Dict(inner) = me {
            assert_eq!(inner.brand.as_deref(), Some("Email"));
        } else {
            panic!()
        }
    }
}
