//! review-improvement-174 regression tests for the `#import "..."
//! sha256:"..."` integrity pin verification performed by the
//! evaluator's import path.
//!
//! These tests pin the analyzer-bypass attack surface: a host that
//! parses + evaluates without going through `analyze_entry` must still
//! refuse a module body whose digest disagrees with its inline pin.
//!
//! Each test wires a custom in-memory `ModuleResolver` so the body
//! bytes the evaluator sees are deterministic regardless of the host's
//! filesystem layout — the verification logic operates on those bytes
//! exactly as a remote fetch would.

use super::*;
use relon_eval_api::module::{ModuleResolver, ModuleSource};
use std::sync::Arc;

/// In-memory resolver that maps a single path to a fixed body. Lets the
/// test pin the exact bytes hashed and avoids any disk/network I/O.
struct StubResolver {
    path: String,
    body: String,
}

impl ModuleResolver for StubResolver {
    fn resolve(
        &self,
        path: &str,
        _scope: &Arc<Scope>,
        _range: relon_parser::TokenRange,
    ) -> Result<Option<ModuleSource>, RuntimeError> {
        if path == self.path {
            Ok(Some(ModuleSource {
                canonical_id: format!("stub::{path}"),
                source: self.body.clone(),
                current_dir: String::new(),
            }))
        } else {
            Ok(None)
        }
    }
}

fn parse(source: &str) -> relon_parser::Node {
    relon_parser::parse_document(source).expect("parse")
}

/// Build an evaluation context wired with `StubResolver` so the
/// evaluator picks up our in-memory module for `path`. The context is
/// otherwise the default trusted one (so `#import` is not denied by
/// the sandbox before integrity checks can run).
fn ctx_with_stub(path: &str, body: &str) -> Context {
    let mut ctx = Context::default();
    // Prepend so the stub wins over the default filesystem resolver.
    ctx.module_resolvers
        .insert(
            0,
            Arc::new(StubResolver {
                path: path.to_string(),
                body: body.to_string(),
            }),
        );
    ctx
}

fn run(source: &str, stub_path: &str, stub_body: &str) -> Result<Value, RuntimeError> {
    let node = parse(source);
    let ctx = ctx_with_stub(stub_path, stub_body).with_root(node);
    let ctx = Arc::new({
        let mut ctx = ctx;
        crate::TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    TreeWalkEvaluator::new(Arc::clone(&ctx)).eval_root(&Arc::new(Scope::default()))
}

#[test]
fn import_with_matching_sha256_pin_loads() {
    // sha256("{ value: 42 }") = 18b86269e2161e03ef55c6e067ff28da71e1453456dab03675cee416e4bde8da
    let body = "{ value: 42 }";
    let pin = "18b86269e2161e03ef55c6e067ff28da71e1453456dab03675cee416e4bde8da";
    let src = format!(r#"#import lib from "stub.relon" sha256:"{pin}"
{{ v: lib.value }}"#);
    let result = run(&src, "stub.relon", body).expect("matching pin should evaluate");
    let Value::Dict(d) = result else {
        panic!("expected dict, got {result:?}");
    };
    assert_eq!(d.map.get("v").unwrap(), &Value::Int(42));
}

#[test]
fn import_with_mismatched_sha256_pin_is_rejected() {
    // Body the host serves is *not* what the pin asserts. The pin
    // below is sha256 of "{ value: 42 }", but we feed a different
    // body — the evaluator must drop the import before it parses
    // the body and before any binding is exposed.
    let attacker_body = r#"{ secret: "tampered" }"#;
    let honest_pin = "18b86269e2161e03ef55c6e067ff28da71e1453456dab03675cee416e4bde8da";
    let src = format!(r#"#import lib from "stub.relon" sha256:"{honest_pin}"
{{ v: lib.secret }}"#);
    let err = run(&src, "stub.relon", attacker_body).expect_err("mismatch must error");
    match err {
        RuntimeError::ImportHashMismatch { payload, .. } => {
            assert_eq!(payload.path, "stub.relon");
            assert_eq!(payload.algorithm, "sha256");
            assert_eq!(payload.expected, honest_pin);
            // sha256("{ secret: \"tampered\" }")
            assert_eq!(
                payload.got,
                "ba179d3c25285a816781604fa06705bbe1051fec89e24f2592de4942ce1297de"
            );
        }
        other => panic!("expected ImportHashMismatch, got {other:?}"),
    }
}

#[test]
fn import_with_unsupported_algorithm_is_rejected() {
    // `md5:` is not a recognised algorithm: the parser preserves
    // the identifier verbatim, the evaluator must fail-closed instead
    // of treating "unknown algorithm" as "no pin".
    let body = "{ value: 42 }";
    let src = r#"#import lib from "stub.relon" md5:"d41d8cd98f00b204e9800998ecf8427e"
{ v: lib.value }"#;
    let err = run(src, "stub.relon", body).expect_err("unknown algo must error");
    match err {
        RuntimeError::ImportHashUnknownAlgorithm { path, algorithm, .. } => {
            assert_eq!(path, "stub.relon");
            assert_eq!(algorithm, "md5");
        }
        other => panic!("expected ImportHashUnknownAlgorithm, got {other:?}"),
    }
}

#[test]
fn import_with_malformed_hex_pin_is_rejected() {
    // 8 hex chars where 64 are required for sha256. The analyzer
    // surfaces the same condition for the workspace path; the
    // evaluator now matches for the analyzer-bypass path.
    let body = "{ value: 42 }";
    let src = r#"#import lib from "stub.relon" sha256:"abcd1234"
{ v: lib.value }"#;
    let err = run(src, "stub.relon", body).expect_err("malformed hex must error");
    match err {
        RuntimeError::ImportHashInvalidHex {
            path,
            algorithm,
            expected_len,
            got_len,
            ..
        } => {
            assert_eq!(path, "stub.relon");
            assert_eq!(algorithm, "sha256");
            assert_eq!(expected_len, 64);
            assert_eq!(got_len, 8);
        }
        other => panic!("expected ImportHashInvalidHex, got {other:?}"),
    }
}

#[test]
fn import_with_uppercase_hex_pin_matches_lowercase_digest() {
    // Pins are usually lower-case but the comparison is
    // case-insensitive (matching the analyzer's `digest_matches`),
    // so a copy-pasted upper-case digest must still verify. Pinning
    // this here keeps the two verification sites from drifting on
    // the casing rule.
    let body = "{ value: 42 }";
    let pin_upper = "18B86269E2161E03EF55C6E067FF28DA71E1453456DAB03675CEE416E4BDE8DA";
    let src = format!(r#"#import lib from "stub.relon" sha256:"{pin_upper}"
{{ v: lib.value }}"#);
    let result = run(&src, "stub.relon", body).expect("upper-case pin must verify");
    let Value::Dict(d) = result else {
        panic!("expected dict, got {result:?}");
    };
    assert_eq!(d.map.get("v").unwrap(), &Value::Int(42));
}

#[test]
fn import_without_pin_still_loads() {
    // Pinning is opt-in: a `#import` without an integrity clause
    // must keep working exactly like before this fix. Regression
    // guard against accidentally turning the pin field into a
    // hard requirement.
    let body = "{ value: 42 }";
    let src = r#"#import lib from "stub.relon"
{ v: lib.value }"#;
    let result = run(src, "stub.relon", body).expect("unpinned import still works");
    let Value::Dict(d) = result else {
        panic!("expected dict, got {result:?}");
    };
    assert_eq!(d.map.get("v").unwrap(), &Value::Int(42));
}
