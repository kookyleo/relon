//! `EvaluatorBuilder` example.
//!
//! Demonstrates the open-the-box construction path for hosts that
//! want more than `relon::from_str` exposes: selecting a backend,
//! flipping trust posture, and (on the tree-walker only) registering
//! host-supplied native fns the script can call.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p relon --example use_builder
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use relon::{Backend, EvaluatorBuilder, TrustLevel, Value};
use relon_eval_api::{NativeArgs, RelonFunction, RuntimeError};
use relon_parser::TokenRange;

/// Pure host fn the script will call as `host_double(x)`. Doubles an
/// `Int`; surfaces `RuntimeError::TypeMismatch` for anything else
/// (the simplest tree-walker-friendly shape — real hosts usually
/// reuse the dispatch helpers shipped in `relon-evaluator`'s
/// `native_fn` module instead of building this by hand).
struct HostDouble;

impl RelonFunction for HostDouble {
    fn call(&self, args: NativeArgs, range: TokenRange) -> Result<Value, RuntimeError> {
        let positional = args.into_positional();
        match positional.as_slice() {
            [Value::Int(n)] => Ok(Value::Int(n * 2)),
            [other] => Err(RuntimeError::TypeMismatch {
                expected: "Int".into(),
                found: other.type_name().to_string(),
                range,
            }),
            _ => Err(RuntimeError::TypeMismatch {
                expected: "exactly 1 argument".into(),
                found: format!("{} arguments", positional.len()),
                range,
            }),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Source declares a `#main(Int x)` entry program that calls our
    // host fn through the normal name-binding path. The builder
    // stages the registration; `.build()` applies it to the
    // tree-walker `Context` it constructs.
    let source = "#main(Int x) -> Int\nhost_double(x) + 1";

    let evaluator = EvaluatorBuilder::from_str(source)
        // Default is `Backend::Auto`; pin to `TreeWalk` because host
        // fn registration is only meaningful on that backend.
        .backend(Backend::TreeWalk)
        // Default is `Sandboxed`. Trusted unlocks filesystem
        // `#import` and capability-gated native fns; this example
        // doesn't need it but the flip is one line.
        .trust(TrustLevel::Sandboxed)
        .register_pure_native_fn("host_double", Arc::new(HostDouble))
        .build()?;

    // Pass `#main(...)` args through a name → Value map. The
    // tree-walker validates them against the declared signature
    // before dispatch.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(20));
    let result = evaluator.run_main(args)?;

    println!("host_double(20) + 1 = {result:?}");

    Ok(())
}
