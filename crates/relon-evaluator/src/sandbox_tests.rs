//! Capability / sandbox layer (Capabilities + FilesystemModuleResolver
//! root + step counter + value-size watermark + register_fn_with_caps).
//!
//! Each test pins one knob; together they pin down the spec from
//! `tmp/critical-analysis-round2.md`.
//!
//! Extracted from lib.rs and wired in via `#[cfg(test)] mod sandbox_tests;`.

use super::*;
use std::sync::Arc;

fn parse(source: &str) -> relon_parser::Node {
    relon_parser::parse_document(source).expect("parse")
}

fn eval_with(ctx: Context, source: &str) -> Result<Value, RuntimeError> {
    let node = parse(source);
    let ctx = ctx.with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()))
}

#[test]
fn sandboxed_context_rejects_default_filesystem_imports() {
    // Sandboxed `Context` ships a default-rejecting filesystem
    // resolver, so any non-`std/` import must fail with
    // `CapabilityDenied` before the OS is touched.
    let dir = std::env::temp_dir().join(format!("relon-sbox-default-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("lib.relon"), r#"{ secret: "leak" }"#).unwrap();

    let node = parse(r#"#import lib from "lib.relon" { x: lib.secret }"#);
    let ctx = Context::sandboxed().with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    let scope = Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&scope);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        matches!(&result, Err(RuntimeError::CapabilityDenied { name, .. }) if name.contains("#import")),
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
    let node = parse(r#"#import lib from "lib.relon" { v: lib.value }"#);
    let ctx = ctx.with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    let scope = Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });

    let result = Evaluator::new(std::sync::Arc::clone(&ctx))
        .eval_root(&scope)
        .unwrap();
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
    let node = parse(r#"#import x from "../escape.relon" { y: x.leak }"#);
    let ctx = ctx.with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    let scope = Arc::new(Scope {
        current_dir: inner.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&scope);
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
    let node = parse(r#"#import x from "link.relon" { y: x.leak }"#);
    let ctx = ctx.with_root(node);
    let ctx = std::sync::Arc::new(ctx);
    let scope = Arc::new(Scope {
        current_dir: inner.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&scope);
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
    ctx.register_fn_with_caps("fs.read", NativeFnGate { reads_fs: true }, Arc::new(ReadFs));
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
    ctx.register_fn_with_caps("fs.read", NativeFnGate { reads_fs: true }, Arc::new(ReadFs));
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
    ctx.register_fn_with_caps("fs.read", NativeFnGate { reads_fs: true }, Arc::new(ReadFs));
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
        r#"#import list from "std/list"
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
        #schema Page<T>: {
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
    let ctx = std::sync::Arc::new(ctx);
    let evaluator = Evaluator::new(std::sync::Arc::clone(&ctx));
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

    // Usage site doesn't define 'Email', but uses it via #brand
    let src = r#"{
        #brand Email
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
