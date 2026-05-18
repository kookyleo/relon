//! Shared test scaffolding for the emitter integration tests.
//!
//! Two utilities live here:
//!
//! * [`emit_and_verify`] — feed an `OptimizedTrace` through the
//!   emitter and run cranelift's `verify_function` on the result.
//! * [`default_flags`] — minimal `settings::Flags` good enough for
//!   the verifier (no target ISA needed since we only check the
//!   IR-level invariants).

use cranelift_codegen::settings;
use cranelift_codegen::verifier;
use cranelift_codegen::Context;
use relon_trace_emitter::TraceEmitter;
use relon_trace_jit::OptimizedTrace;

pub fn default_flags() -> settings::Flags {
    settings::Flags::new(settings::builder())
}

/// Run the emitter then the verifier. Panics on emit or verify
/// failure so the test reports a useful trace.
pub fn emit_and_verify(trace: &OptimizedTrace) -> Context {
    let mut ctx = Context::new();
    TraceEmitter::emit(trace, &mut ctx).expect("emit should succeed");
    let flags = default_flags();
    if let Err(errors) = verifier::verify_function(&ctx.func, &flags) {
        panic!(
            "cranelift verifier rejected the emitted function:\n{}\n--- IR ---\n{}",
            errors, ctx.func
        );
    }
    ctx
}
