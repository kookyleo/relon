//! Host-boundary error surfacing.
//!
//! Each runtime error a script can raise must reach the host as a
//! typed `Err(RuntimeError)` from `eval_root` / `run_main`, with
//! enough metadata for the host to (a) pattern-match the variant
//! programmatically, (b) render a diagnostic via `miette` for users.
//! The cases below pair a minimal triggering script with assertions
//! on the variant and on the source-location label.

use crate::eval::{Capabilities, Context, Evaluator};
use crate::module::FilesystemModuleResolver;
use crate::native_fn::{NativeArgs, RelonFunction};
use crate::scope::Scope;
use crate::value::Value;
use crate::{NativeFnGate, RuntimeError};
use relon_parser::{parse_document, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;

fn run(source: &str) -> Result<Value, RuntimeError> {
    let node = parse_document(source).expect("parse");
    let ctx = Context::new().with_root(node);
    Evaluator::new(Arc::new(ctx)).eval_root(&Arc::new(Scope::default()))
}

fn run_with_caps(source: &str, caps: Capabilities) -> Result<Value, RuntimeError> {
    let node = parse_document(source).expect("parse");
    let mut ctx = Context::new().with_root(node);
    ctx.capabilities = caps;
    Evaluator::new(Arc::new(ctx)).eval_root(&Arc::new(Scope::default()))
}

/// Exercise miette's `Diagnostic` rendering on the error path so any
/// missing `#[label]` or `#[source_code]` wiring shows up here rather
/// than crashing the host's own logging pipeline.
fn render_does_not_panic(err: &RuntimeError) {
    let report = miette::Report::new_boxed(Box::new(err.clone()));
    let _ = format!("{report:?}");
}

// `miette::Report::new` requires `Diagnostic + Send + Sync + 'static`;
// `RuntimeError` is `Clone` so we can hand a fresh copy to the report.
impl Clone for crate::error::RuntimeError {
    fn clone(&self) -> Self {
        // Re-roundtrip through Debug + a coarse `From<&str>` is overkill;
        // RuntimeError is plain data, so a naive recursive clone suffices.
        // We mark this `Clone` impl as an opt-in helper for tests only —
        // production code uses references.
        match self {
            Self::VariableNotFound(s, r) => Self::VariableNotFound(s.clone(), *r),
            Self::TypeMismatch {
                expected,
                found,
                range,
            } => Self::TypeMismatch {
                expected: expected.clone(),
                found: found.clone(),
                range: *range,
            },
            Self::ValidationError(s, r) => Self::ValidationError(s.clone(), *r),
            Self::DivisionByZero(r) => Self::DivisionByZero(*r),
            Self::FunctionNotFound(s, r) => Self::FunctionNotFound(s.clone(), *r),
            Self::CircularReference { cycle, range } => Self::CircularReference {
                cycle: cycle.clone(),
                range: *range,
            },
            Self::UnsupportedOperator(s, r) => Self::UnsupportedOperator(s.clone(), *r),
            Self::InvalidIdentifier(s, r) => Self::InvalidIdentifier(s.clone(), *r),
            Self::IoError(s) => Self::IoError(s.clone()),
            Self::ModuleNotFound(s, r) => Self::ModuleNotFound(s.clone(), *r),
            Self::ModuleParseError {
                path,
                message,
                range,
            } => Self::ModuleParseError {
                path: path.clone(),
                message: message.clone(),
                range: *range,
            },
            Self::CircularImport(v, r) => Self::CircularImport(v.clone(), *r),
            Self::NumericOverflow(r) => Self::NumericOverflow(*r),
            Self::StepLimitExceeded { limit, range } => Self::StepLimitExceeded {
                limit: *limit,
                range: *range,
            },
            Self::RecursionLimitExceeded { limit, range } => Self::RecursionLimitExceeded {
                limit: *limit,
                range: *range,
            },
            Self::ValueTooLarge {
                limit,
                actual,
                range,
            } => Self::ValueTooLarge {
                limit: *limit,
                actual: *actual,
                range: *range,
            },
            Self::CapabilityDenied {
                name,
                reason,
                range,
            } => Self::CapabilityDenied {
                name: name.clone(),
                reason: reason.clone(),
                range: *range,
            },
            Self::NoMainSignature { range } => Self::NoMainSignature { range: *range },
            Self::MissingMainArg { name, range } => Self::MissingMainArg {
                name: name.clone(),
                range: *range,
            },
            Self::UnexpectedMainArg { name, range } => Self::UnexpectedMainArg {
                name: name.clone(),
                range: *range,
            },
            Self::MainArgTypeMismatch {
                name,
                expected,
                found,
                range,
            } => Self::MainArgTypeMismatch {
                name: name.clone(),
                expected: expected.clone(),
                found: found.clone(),
                range: *range,
            },
            Self::MainReturnTypeMismatch {
                expected,
                found,
                range,
            } => Self::MainReturnTypeMismatch {
                expected: expected.clone(),
                found: found.clone(),
                range: *range,
            },
        }
    }
}

fn assert_range_pinned(range: TokenRange, kind: &str) {
    let default_range = TokenRange::default();
    assert!(
        range != default_range,
        "{kind}: TokenRange should locate to the offending source token, got default"
    );
}

#[test]
fn host_receives_variable_not_found() {
    let result = run(r#"{ x: undefined_name }"#);
    let err = result.expect_err("should error");
    let RuntimeError::VariableNotFound(name, range) = &err else {
        panic!("expected VariableNotFound, got {err:?}");
    };
    assert_eq!(name, "undefined_name");
    assert_range_pinned(*range, "VariableNotFound");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_type_mismatch() {
    let result = run(r#"{ Int x: "not a number" }"#);
    let err = result.expect_err("should error");
    let RuntimeError::TypeMismatch {
        expected,
        found,
        range,
    } = &err
    else {
        panic!("expected TypeMismatch, got {err:?}");
    };
    assert_eq!(expected, "Int");
    assert_eq!(found, "String");
    assert_range_pinned(*range, "TypeMismatch");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_division_by_zero() {
    let result = run(r#"{ x: 1 / 0 }"#);
    let err = result.expect_err("should error");
    let RuntimeError::DivisionByZero(range) = &err else {
        panic!("expected DivisionByZero, got {err:?}");
    };
    assert_range_pinned(*range, "DivisionByZero");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_numeric_overflow() {
    let result = run(r#"{ x: 9223372036854775807 + 1 }"#);
    let err = result.expect_err("should error");
    let RuntimeError::NumericOverflow(range) = &err else {
        panic!("expected NumericOverflow, got {err:?}");
    };
    assert_range_pinned(*range, "NumericOverflow");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_circular_reference() {
    // Two mutually-referential sibling fields form a cycle.
    let result = run(r#"{
            a: &sibling.b,
            b: &sibling.a
        }"#);
    let err = result.expect_err("should error");
    let RuntimeError::CircularReference { cycle, range } = &err else {
        panic!("expected CircularReference, got {err:?}");
    };
    assert!(!cycle.is_empty(), "cycle path should be non-empty");
    assert_range_pinned(*range, "CircularReference");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_step_limit_exceeded() {
    // Tight budget plus a loop that easily blows past it.
    let mut caps = Capabilities::all_granted();
    caps.max_steps = Some(20);
    let result = run_with_caps(
        r#"{
            f(n): n <= 0 ? 0 : f(n - 1) + 1,
            x: f(50)
        }"#,
        caps,
    );
    let err = result.expect_err("should error");
    let RuntimeError::StepLimitExceeded { limit, range } = &err else {
        panic!("expected StepLimitExceeded, got {err:?}");
    };
    assert_eq!(*limit, 20);
    assert_range_pinned(*range, "StepLimitExceeded");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_value_too_large() {
    // Build a list whose element count exceeds the bound.
    let mut caps = Capabilities::all_granted();
    caps.max_value_elements = Some(2);
    let result = run_with_caps(r#"[1, 2, 3, 4, 5]"#, caps);
    let err = result.expect_err("should error");
    let RuntimeError::ValueTooLarge {
        limit,
        actual,
        range,
    } = &err
    else {
        panic!("expected ValueTooLarge, got {err:?}");
    };
    assert_eq!(*limit, 2);
    assert!(*actual > 2);
    assert_range_pinned(*range, "ValueTooLarge");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_capability_denied() {
    // A native function gated on `reads_fs` is rejected by a sandboxed
    // (zero-grant) Context. The `Capabilities::default` Context has
    // neither `reads_fs` nor an entry in `allow_native_fn` for this fn.
    struct ReadsFs;
    impl RelonFunction for ReadsFs {
        fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }

    let node = parse_document(r#"{ x: read_file() }"#).expect("parse");
    let mut ctx = Context::sandboxed().with_root(node);
    ctx.register_fn(
        "read_file",
        NativeFnGate {
            reads_fs: true,
            ..NativeFnGate::default()
        },
        Arc::new(ReadsFs),
    );
    let result = Evaluator::new(Arc::new(ctx)).eval_root(&Arc::new(Scope::default()));

    let err = result.expect_err("should error");
    let RuntimeError::CapabilityDenied { name, range, .. } = &err else {
        panic!("expected CapabilityDenied, got {err:?}");
    };
    assert_eq!(name, "read_file");
    assert_range_pinned(*range, "CapabilityDenied");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_no_main_signature() {
    // Calling `run_main` on a file without `#main(...)` must surface
    // `NoMainSignature`, not a generic error or a panic.
    let node = parse_document(r#"{ x: 1 }"#).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let result =
        Evaluator::new(Arc::new(ctx)).run_main(&Arc::new(Scope::default()), HashMap::new());

    let err = result.expect_err("should error");
    let RuntimeError::NoMainSignature { range } = &err else {
        panic!("expected NoMainSignature, got {err:?}");
    };
    assert_range_pinned(*range, "NoMainSignature");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_main_arg_type_mismatch() {
    // Host pushes a value whose type doesn't match the declared
    // parameter — surface `MainArgTypeMismatch` cleanly, not a generic
    // `TypeMismatch`.
    let source = r#"#main(Int n)
{ ok: n + 1 }"#;
    let node = parse_document(source).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::String("oops".into()));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let result = Evaluator::new(Arc::new(ctx)).run_main(&Arc::new(Scope::default()), args);

    let err = result.expect_err("should error");
    let RuntimeError::MainArgTypeMismatch { name, range, .. } = &err else {
        panic!("expected MainArgTypeMismatch, got {err:?}");
    };
    assert_eq!(name, "n");
    assert_range_pinned(*range, "MainArgTypeMismatch");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_main_return_type_mismatch() {
    // Body returns a Dict; signature declares `-> String`. Surface
    // `MainReturnTypeMismatch`, not a raw `TypeMismatch`.
    let source = r#"#main(Int n) -> String
{ result: n + 1 }"#;
    let node = parse_document(source).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut args: HashMap<String, Value> = HashMap::new();
    args.insert("n".to_string(), Value::Int(1));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    let result = Evaluator::new(Arc::new(ctx)).run_main(&Arc::new(Scope::default()), args);

    let err = result.expect_err("should error");
    let RuntimeError::MainReturnTypeMismatch {
        expected, range, ..
    } = &err
    else {
        panic!("expected MainReturnTypeMismatch, got {err:?}");
    };
    assert_eq!(expected, "String");
    assert_range_pinned(*range, "MainReturnTypeMismatch");
    render_does_not_panic(&err);
}

#[test]
fn host_receives_module_not_found() {
    // `#import` against a path no resolver can find. The default
    // `Context::new` has the trusted FS resolver; a missing file
    // surfaces `ModuleNotFound` with the import-site source span.
    let dir = std::env::temp_dir().join(format!("relon-host-mod-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let node = parse_document(
        r#"#import missing from "./does-not-exist.relon"
            { x: 1 }"#,
    )
    .expect("parse");
    let mut ctx = Context::new().with_root(node);
    ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
    let scope = Arc::new(Scope {
        current_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = Evaluator::new(Arc::new(ctx)).eval_root(&scope);
    let _ = std::fs::remove_dir_all(&dir);

    let err = result.expect_err("should error");
    assert!(
        matches!(&err, RuntimeError::ModuleNotFound(_, _)),
        "expected ModuleNotFound, got {err:?}"
    );
    render_does_not_panic(&err);
}

#[test]
fn host_can_render_diagnostic_with_source_for_user() {
    // Smoke test the host's typical "render the error for the user"
    // path: build a Report with attached source, format it, and
    // confirm the formatted output mentions the offending range.
    let source = r#"{ x: undefined }"#;
    let node = parse_document(source).expect("parse");
    let ctx = Context::new().with_root(node);
    let err = Evaluator::new(Arc::new(ctx))
        .eval_root(&Arc::new(Scope::default()))
        .expect_err("should error");

    let report = miette::Report::new(err).with_source_code(source.to_string());
    let rendered = format!("{report:?}");
    // miette's Debug impl walks the labels — if any label range is
    // out of bounds we'd see the panic here, not in production.
    assert!(
        rendered.contains("undefined"),
        "rendered diagnostic should reference the offending token, got:\n{rendered}"
    );
}

#[test]
fn host_receives_capability_denied_for_each_new_bit() {
    // Each of the 5 new capability bits — `writes_fs`, `network`,
    // `reads_clock`, `reads_env`, `uses_rng` — must surface the same
    // typed `CapabilityDenied` shape as `reads_fs`, with `reason`
    // mentioning the specific bit. Drives runtime's
    // `check_native_fn_capability` table-driven walk.
    struct Stub;
    impl RelonFunction for Stub {
        fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }

    let cases: Vec<(&str, NativeFnGate)> = vec![
        (
            "writes_fs",
            NativeFnGate {
                writes_fs: true,
                ..NativeFnGate::default()
            },
        ),
        (
            "network",
            NativeFnGate {
                network: true,
                ..NativeFnGate::default()
            },
        ),
        (
            "reads_clock",
            NativeFnGate {
                reads_clock: true,
                ..NativeFnGate::default()
            },
        ),
        (
            "reads_env",
            NativeFnGate {
                reads_env: true,
                ..NativeFnGate::default()
            },
        ),
        (
            "uses_rng",
            NativeFnGate {
                uses_rng: true,
                ..NativeFnGate::default()
            },
        ),
    ];

    for (bit, gate) in cases {
        let node = parse_document(r#"{ x: f() }"#).expect("parse");
        let mut ctx = Context::sandboxed().with_root(node);
        ctx.register_fn("f", gate, Arc::new(Stub));
        let result = Evaluator::new(Arc::new(ctx)).eval_root(&Arc::new(Scope::default()));
        let err = result.expect_err(&format!("bit `{bit}` should error"));
        let RuntimeError::CapabilityDenied { name, reason, .. } = &err else {
            panic!("bit `{bit}`: expected CapabilityDenied, got {err:?}");
        };
        assert_eq!(name, "f", "bit `{bit}`: name");
        assert!(
            reason.contains(bit),
            "bit `{bit}`: reason should mention `{bit}`, got `{reason}`"
        );
    }
}

#[test]
fn capability_denied_reports_first_missing_bit() {
    // Fn declares `reads_fs + network`; host grants `reads_fs` only.
    // Runtime stops at the first miss in `NativeFnGate`'s
    // field-declaration order (reads_fs first, network second). With
    // reads_fs already granted, the report names `network`.
    struct Stub;
    impl RelonFunction for Stub {
        fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
            Ok(Value::Null)
        }
    }

    let node = parse_document(r#"{ x: fetch() }"#).expect("parse");
    let mut ctx = Context::sandboxed().with_root(node);
    ctx.capabilities.reads_fs = true; // grant one of the two
    ctx.register_fn(
        "fetch",
        NativeFnGate {
            reads_fs: true,
            network: true,
            ..NativeFnGate::default()
        },
        Arc::new(Stub),
    );
    let err = Evaluator::new(Arc::new(ctx))
        .eval_root(&Arc::new(Scope::default()))
        .expect_err("should error on the ungranted bit");
    let RuntimeError::CapabilityDenied { reason, .. } = &err else {
        panic!("expected CapabilityDenied, got {err:?}");
    };
    assert!(
        reason.contains("network"),
        "reason should mention the ungranted bit `network`, got `{reason}`"
    );
    assert!(
        !reason.contains("reads_fs"),
        "reason should not mention the already-granted bit `reads_fs`, got `{reason}`"
    );
}

#[test]
fn pure_fn_passes_under_full_sandbox() {
    // `register_pure_fn` declares the empty gate. Even under a fully
    // sandboxed Context (no allowlist, no granted bits) the call
    // succeeds — the all-zero gate is trivially satisfied.
    struct Pure;
    impl RelonFunction for Pure {
        fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
            Ok(Value::Int(42))
        }
    }
    let node = parse_document(r#"{ x: deterministic() }"#).expect("parse");
    let mut ctx = Context::sandboxed().with_root(node);
    ctx.register_pure_fn("deterministic", Arc::new(Pure));
    let result = Evaluator::new(Arc::new(ctx))
        .eval_root(&Arc::new(Scope::default()))
        .expect("pure fn should pass under full sandbox");
    let Value::Dict(d) = result else {
        panic!("expected dict")
    };
    assert_eq!(d.map.get("x"), Some(&Value::Int(42)));
}
