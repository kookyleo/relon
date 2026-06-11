//! Host-boundary error surfacing.
//!
//! Each runtime error a script can raise must reach the host as a
//! typed `Err(RuntimeError)` from `eval_root` / `run_main`, with
//! enough metadata for the host to (a) pattern-match the variant
//! programmatically, (b) render a diagnostic via `miette` for users.
//! The cases below pair a minimal triggering script with assertions
//! on the variant and on the source-location label.

use crate::eval::TreeWalkEvaluator;
use crate::module::FilesystemModuleResolver;
use crate::native_fn::{NativeArgs, RelonFunction};
use crate::scope::Scope;
use crate::value::Value;
use crate::{NativeFnGate, RuntimeError};
use relon_eval_api::context::{Capabilities, Context};
use relon_parser::{parse_document, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;

fn run(source: &str) -> Result<Value, RuntimeError> {
    let node = parse_document(source).expect("parse");
    let ctx = Context::new().with_root(node);
    TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .eval_root(&Arc::new(Scope::default()))
}

fn run_with_caps(source: &str, caps: Capabilities) -> Result<Value, RuntimeError> {
    let node = parse_document(source).expect("parse");
    let ctx = Context::new().with_root(node).with_capabilities(caps);
    TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .eval_root(&Arc::new(Scope::default()))
}

/// Exercise miette's `Diagnostic` rendering on the error path so any
/// missing `#[label]` or `#[source_code]` wiring shows up here rather
/// than crashing the host's own logging pipeline.
fn render_does_not_panic(err: &RuntimeError) {
    let report = miette::Report::new_boxed(Box::new(err.clone()));
    let _ = format!("{report:?}");
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
    assert_eq!(*limit, Some(20));
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
    // neither `reads_fs` nor any other path that would satisfy the
    // gate.
    struct ReadsFs;
    impl RelonFunction for ReadsFs {
        fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
            Ok(Value::option_none())
        }
    }

    let node = parse_document(r#"{ x: host_read() }"#).expect("parse");
    let mut ctx = Context::sandboxed().with_root(node);
    ctx.register_fn(
        "host_read",
        {
            // `NativeFnGate` is `#[non_exhaustive]` (defined in
            // `relon-cap`); build via default + field set.
            let mut g = NativeFnGate::default();
            g.reads_fs = true;
            g
        },
        Arc::new(ReadsFs),
    );
    let result = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .eval_root(&Arc::new(Scope::default()));

    let err = result.expect_err("should error");
    let RuntimeError::CapabilityDenied { reason, range, .. } = &err else {
        panic!("expected CapabilityDenied, got {err:?}");
    };
    assert!(reason.contains("host_read"), "reason: {reason}");
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
    let result = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .run_main(&Arc::new(Scope::default()), HashMap::new());

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
    let result = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .run_main(&Arc::new(Scope::default()), args);

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
    let result = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .run_main(&Arc::new(Scope::default()), args);

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
        current_dir: dir.to_string_lossy().into_owned().into(),
        ..Default::default()
    });
    let result = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .eval_root(&scope);
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
    let err = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
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
            Ok(Value::option_none())
        }
    }

    // `NativeFnGate` is `#[non_exhaustive]` (defined in `relon-cap`), so
    // each gate is built via default + a single field set.
    let writes_fs = {
        let mut g = NativeFnGate::default();
        g.writes_fs = true;
        g
    };
    let network = {
        let mut g = NativeFnGate::default();
        g.network = true;
        g
    };
    let reads_clock = {
        let mut g = NativeFnGate::default();
        g.reads_clock = true;
        g
    };
    let reads_env = {
        let mut g = NativeFnGate::default();
        g.reads_env = true;
        g
    };
    let uses_rng = {
        let mut g = NativeFnGate::default();
        g.uses_rng = true;
        g
    };
    let cases: Vec<(&str, NativeFnGate)> = vec![
        ("writes_fs", writes_fs),
        ("network", network),
        ("reads_clock", reads_clock),
        ("reads_env", reads_env),
        ("uses_rng", uses_rng),
    ];

    for (bit, gate) in cases {
        let node = parse_document(r#"{ x: f() }"#).expect("parse");
        let mut ctx = Context::sandboxed().with_root(node);
        ctx.register_fn("f", gate, Arc::new(Stub));
        let result = TreeWalkEvaluator::new(Arc::new({
            let mut ctx = ctx;
            crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
            ctx
        }))
        .eval_root(&Arc::new(Scope::default()));
        let err = result.expect_err(&format!("bit `{bit}` should error"));
        let RuntimeError::CapabilityDenied { reason, .. } = &err else {
            panic!("bit `{bit}`: expected CapabilityDenied, got {err:?}");
        };
        assert!(
            reason.contains("`f`"),
            "bit `{bit}`: reason should name the fn, got `{reason}`"
        );
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
            Ok(Value::option_none())
        }
    }

    let node = parse_document(r#"{ x: fetch() }"#).expect("parse");
    let mut ctx = Context::sandboxed().with_root(node);
    let mut caps = ctx.capabilities().clone();
    caps.reads_fs = true; // grant one of the two
    ctx = ctx.with_capabilities(caps);
    ctx.register_fn(
        "fetch",
        {
            // `NativeFnGate` is `#[non_exhaustive]` (defined in
            // `relon-cap`); build via default + field set.
            let mut g = NativeFnGate::default();
            g.reads_fs = true;
            g.network = true;
            g
        },
        Arc::new(Stub),
    );
    let err = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
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
    let result = TreeWalkEvaluator::new(Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    }))
    .eval_root(&Arc::new(Scope::default()))
    .expect("pure fn should pass under full sandbox");
    let Value::Dict(d) = result else {
        panic!("expected dict")
    };
    assert_eq!(d.map.get("x"), Some(&Value::Int(42)));
}

/// Cross-check that every `#native` method slot declared in the
/// analyzer-side `core/*.relon` carriers has a matching runtime
/// registration in `stdlib::register_to`. Decision 21' (carrier model)
/// notes this risk: a `#native` slot added to `core/list.relon` without
/// the corresponding `register_pure_method("List", "...", ...)` in the
/// evaluator looks fine at analysis (dispatch resolves to a slot) but
/// surfaces as `FunctionNotFound` at evaluation. Pinning the invariant
/// at test time turns the silent drift into a noisy CI failure.
///
/// We accumulate every missing pair before panicking so one fix per
/// drift covers the full delta — no whack-a-mole round trips.
///
/// Only the forward direction is asserted (`core/*.relon` ⇒ stdlib).
/// The reverse direction (stdlib ⇒ core/*.relon) is intentionally
/// skipped: `register_pure_method` also services internal aliases
/// (e.g. `String.len` shares the polymorphic `Len` intrinsic) where the
/// runtime table is the source of truth and the carrier's `#native`
/// slot is the one that documents the API. Asserting reverse would
/// false-flag these legitimate registrations.
#[test]
fn core_relon_native_slots_match_stdlib_registration() {
    // Empty root is fine — `analyze` injects the core carriers regardless.
    let node = parse_document("{}").expect("parse empty doc");
    let tree = relon_analyzer::analyze(&node);

    let mut ctx = Context::new();
    // The native-fn registration happens lazily when a `TreeWalkEvaluator`
    // wraps a bare context (see `prepare_in_place`); this test inspects
    // the registration table directly, so prime it explicitly here.
    crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);

    let mut missing: Vec<(String, String)> = Vec::new();
    for (schema_name, methods) in tree.schema_methods.iter() {
        for method in methods {
            if !method.is_native {
                continue;
            }
            let present = ctx
                .native_methods
                .get(schema_name)
                .is_some_and(|m| m.contains_key(method.name.as_str()));
            if !present {
                missing.push((schema_name.clone(), method.name.clone()));
            }
        }
    }

    if !missing.is_empty() {
        let listing = missing
            .iter()
            .map(|(s, m)| format!("  - ({s}, {m})"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "core/*.relon declares `#native` method(s) without a matching \
             register_pure_method registration in stdlib::register_to:\n{listing}\n\n\
             Adding a `#native` slot in core/*.relon without the matching runtime \
             registration causes FunctionNotFound at evaluation time. Wire the new \
             method through stdlib::register_to so dispatch can land on a real \
             RelonFunction impl."
        );
    }

    // Defensive lower bound: the four built-in carriers (`String`,
    // `List`, `Dict`, `Iter`) collectively declare ~20 native slots
    // today. If the carrier loader silently fails to install any of
    // them, `missing.is_empty()` would still hold (empty universe ⇒
    // vacuous truth), so assert we actually saw a non-trivial number
    // of forward-checked pairs. The bound is loose enough that adding
    // new methods doesn't flake; it just catches the "loader silently
    // returned nothing" regression.
    let checked = tree
        .schema_methods
        .values()
        .flat_map(|v| v.iter())
        .filter(|m| m.is_native)
        .count();
    assert!(
        checked >= 15,
        "expected at least 15 #native slots from core/*.relon carriers, \
         saw {checked} — has inject_core_schemas regressed?"
    );
}
