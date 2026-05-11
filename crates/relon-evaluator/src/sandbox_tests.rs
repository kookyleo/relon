//! Capability / sandbox layer (Capabilities + FilesystemModuleResolver
//! root + step counter + value-size watermark + gated `register_fn`).
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
fn max_value_elements_rejects_oversized_list() {
    // The watermark fires at every language-level constructor:
    // literal lists, dict `+` merge, list-comprehension, and every
    // stdlib intrinsic that returns a `List` / `Dict` (covered by
    // the catch-all in `Evaluator::call_function` /
    // `try_call_native_method`). Cover the literal-list path here;
    // the stdlib-intrinsic cases live in their own dedicated tests
    // below (see `max_value_elements_rejects_range_preflight` etc.).
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(3);
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
fn max_value_elements_rejects_oversized_dict() {
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(2);
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
fn max_value_elements_rejects_range_preflight() {
    // `range(0, N)` with N far above any plausible host RAM must be
    // refused *before* it allocates the underlying `Vec<Value>`. The
    // pre-flight check inside `Range::call` consults
    // `NativeFnCaps::max_value_elements()` and bails on
    // `end - start > cap`. Without it, asking for 10M elements with
    // cap=3 would burn 10M * sizeof(Value) bytes before the post-call
    // `check_value_size` ever ran — a real OOM vector on small hosts.
    // We pick 10_000_000 (not the brief's 10G) so a regression here is
    // visible as a noticeable slowdown / RSS spike on every CI run
    // rather than a hard-kill, while still being unambiguously larger
    // than what the post-call check would let through "for free."
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(3);
    let result = eval_with(ctx, r#"{ x: len(range(0, 10000000)) }"#);
    assert!(
        matches!(
            result,
            Err(RuntimeError::ValueTooLarge {
                limit: 3,
                actual: 10_000_000,
                ..
            })
        ),
        "expected ValueTooLarge, got {result:?}"
    );
}

#[test]
fn max_value_elements_rejects_list_map_result() {
    // `_list_map` is the underscore intrinsic that backs both
    // `list.map(xs, f)` (via the `std/list` virtual module) and
    // `xs.map(f)` (via `register_pure_method("List", "map", ...)`).
    // Both shapes ultimately funnel through `call_function` /
    // `try_call_native_method`, so the catch-all `check_value_size`
    // there is the only enforcement point. Using the underscore form
    // here avoids the `#import` boilerplate while exercising the same
    // dispatch path.
    //
    // Positive baseline: input and output both at cap → success.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(3);
    let result = eval_with(ctx, r#"{ xs: _list_map(range(0, 3), (x) => x * 2) }"#);
    assert!(result.is_ok(), "expected success at cap=3, got {result:?}");

    // Negative: input of 4 trips the cap. The check fires somewhere on
    // the construction chain (range pre-flight, or the map result) —
    // we only care that the system refuses to bind an oversized list.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(3);
    let result = eval_with(ctx, r#"{ xs: _list_map(range(0, 4), (x) => x * 2) }"#);
    assert!(
        matches!(result, Err(RuntimeError::ValueTooLarge { limit: 3, .. })),
        "expected ValueTooLarge with limit=3, got {result:?}"
    );
}

#[test]
fn max_value_elements_rejects_list_filter_result() {
    // `_list_filter` is the same dispatch path as `_list_map`. Even
    // when the filter throws away elements, the *input* list still
    // has to be built first — and the catch-all guards every native-fn
    // return value. We exercise the positive path (input + result both
    // fit) and a negative path where the input exceeds the cap, which
    // is rejected at the list-literal construction site.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(5);
    let result = eval_with(ctx, r#"{ ys: _list_filter(range(0, 5), (x) => x > 0) }"#);
    assert!(result.is_ok(), "expected success at cap=5, got {result:?}");

    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(4);
    let result = eval_with(ctx, r#"{ ys: _list_filter(range(0, 5), (x) => x > 0) }"#);
    assert!(
        matches!(result, Err(RuntimeError::ValueTooLarge { limit: 4, .. })),
        "expected ValueTooLarge with limit=4, got {result:?}"
    );
}

#[test]
fn max_value_elements_rejects_string_split_result() {
    // `_string_split` returns `List<String>`. The catch-all post-call
    // check on `call_function` is the only enforcement point for the
    // result — the input string itself has no element-count semantics
    // under `max_value_elements`. Splitting a 5-piece string under
    // cap=3 must reject with `actual=5`.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(3);
    let result = eval_with(ctx, r#"{ parts: _string_split("a,b,c,d,e", ",") }"#);
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

    // Positive baseline: cap=5 lets the same call through.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(5);
    let result = eval_with(ctx, r#"{ parts: _string_split("a,b,c,d,e", ",") }"#);
    assert!(result.is_ok(), "expected success at cap=5, got {result:?}");
}

#[test]
fn max_value_elements_rejects_dict_merge_method_result() {
    // `_dict_merge` is the underscore intrinsic that services both the
    // `dict.merge(a, b)` free-form (via `std/dict`) and the `d.merge(b)`
    // receiver form (via `register_pure_method("Dict", "merge", ...)`).
    // Both routes funnel through `call_function` /
    // `try_call_native_method`, distinct from the `Dict + Dict`
    // binary-op path (which has its own enforcement site in
    // `arithmetic.rs`). Two 2-key dicts merged into a 4-key result must
    // reject under cap=3.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(3);
    let result = eval_with(
        ctx,
        r#"{
    a: { p: 1, q: 2 },
    b: { r: 3, s: 4 },
    m: _dict_merge(&sibling.a, &sibling.b)
}"#,
    );
    assert!(
        matches!(
            result,
            Err(RuntimeError::ValueTooLarge {
                limit: 3,
                actual: 4,
                ..
            })
        ),
        "expected ValueTooLarge, got {result:?}"
    );

    // Positive baseline: under cap=4 the same merge passes.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(4);
    let result = eval_with(
        ctx,
        r#"{
    a: { p: 1, q: 2 },
    b: { r: 3, s: 4 },
    m: _dict_merge(&sibling.a, &sibling.b)
}"#,
    );
    assert!(result.is_ok(), "expected success at cap=4, got {result:?}");
}

#[test]
fn max_value_elements_allows_within_budget_intrinsics() {
    // Positive coverage for the catch-all: each stdlib intrinsic that
    // produces a List/Dict must still succeed when the result fits the
    // cap. Guards against the catch-all going over-eager and rejecting
    // results whose size is exactly at the limit. We pick a top-level
    // dict with 4 keys (≤ cap=5) so the outermost literal also passes.
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(5);
    let result = eval_with(
        ctx,
        r#"{
    r: range(0, 5),
    mapped: _list_map(range(0, 3), (x) => x * 10),
    filtered: _list_filter(range(0, 5), (x) => x > 0),
    split: _string_split("a,b,c,d,e", ",")
}"#,
    );
    assert!(
        result.is_ok(),
        "intrinsics producing at-cap-sized results must pass, got {result:?}"
    );
}

#[test]
fn max_value_elements_rejects_receiver_method_intrinsic() {
    // The catch-all in `try_call_native_method` (the receiver-side
    // dispatch path) must also enforce `max_value_elements`. This
    // route is taken by `xs.map(f)` / `d.merge(other)` /
    // `s.split(sep)` etc. once the analyzer has resolved the schema
    // tag on the receiver — distinct from the free-form route in
    // `call_function`. We require an `AnalyzedTree` for receiver-side
    // dispatch to fire, so wire it up explicitly.
    //
    // Negative shape: a 5-element source mapped under cap=4. The
    // refusal can fire at either site — the literal / `range`
    // pre-flight or the post-call check on the map result — but the
    // sandbox guarantee is "no `List` larger than cap ever escapes,"
    // so any `ValueTooLarge { limit: 4 }` counts as the cap holding.
    let src = r#"{
    xs: range(0, 5),
    ys: xs.map((x) => x * 2)
}"#;
    let node = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&node);
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(4);
    let ctx = ctx.with_root(node).with_analyzed(Arc::new(analyzed));
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()));
    assert!(
        matches!(result, Err(RuntimeError::ValueTooLarge { limit: 4, .. })),
        "expected ValueTooLarge at receiver path, got {result:?}"
    );

    // Positive baseline at cap=5: `range(0, 5).map(...)` stays within
    // budget. Both the range pre-flight and the receiver-side post-call
    // check must let it through.
    let src = r#"{
    xs: range(0, 5),
    ys: xs.map((x) => x * 2)
}"#;
    let node = relon_parser::parse_document(src).expect("parse");
    let analyzed = relon_analyzer::analyze(&node);
    let mut ctx = Context::sandboxed();
    ctx.capabilities.max_value_elements = Some(5);
    let ctx = ctx.with_root(node).with_analyzed(Arc::new(analyzed));
    let ctx = std::sync::Arc::new(ctx);
    let result = Evaluator::new(std::sync::Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()));
    assert!(
        result.is_ok(),
        "expected receiver-side success at cap=5, got {result:?}"
    );
}

#[test]
fn pure_fn_callable_under_sandbox() {
    // `register_pure_fn` declares an empty `NativeFnGate`. Under a
    // fully sandboxed Context with an empty `allow_native_fn` set the
    // call still goes through — the all-zero gate is trivially
    // satisfied by any `Capabilities`.
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
    ctx.register_pure_fn("echo", Arc::new(Echo));
    let result = eval_with(ctx, r#"{ "x": echo(7) }"#).unwrap();
    let Value::Dict(d) = result else {
        panic!("expected dict")
    };
    assert_eq!(d.map.get("x").unwrap(), &Value::Int(7));
}

#[test]
fn gated_fn_rejected_in_sandbox_without_allowlist() {
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
    ctx.register_fn(
        "fs.read",
        NativeFnGate {
            reads_fs: true,
            ..NativeFnGate::default()
        },
        Arc::new(ReadFs),
    );
    let result = eval_with(ctx, r#"{ "data": fs.read() }"#);
    assert!(
        matches!(&result, Err(RuntimeError::CapabilityDenied { name, .. }) if name == "fs.read"),
        "expected CapabilityDenied, got {result:?}"
    );
}

#[test]
fn gated_fn_permitted_when_in_allowlist() {
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
    ctx.register_fn(
        "fs.read",
        NativeFnGate {
            reads_fs: true,
            ..NativeFnGate::default()
        },
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
    // so even fns with non-empty `NativeFnGate` go through without
    // an explicit allowlist entry.
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
    ctx.register_fn(
        "fs.read",
        NativeFnGate {
            reads_fs: true,
            ..NativeFnGate::default()
        },
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
