use miette::Diagnostic;
use relon_parser::TokenRange;
use thiserror::Error;

/// Render a chain of identifiers/paths joined by `→`. Used by the
/// `CircularReference` and `CircularImport` `Display` impls so the error
/// message reads naturally instead of dumping a debug-formatted `Vec`.
fn format_chain(chain: &[String]) -> String {
    chain.join(" \u{2192} ")
}

#[derive(Error, Debug, Diagnostic, Clone)]
pub enum RuntimeError {
    #[error("Variable not found: {0}")]
    #[diagnostic(
        code(relon::eval::variable_not_found),
        help("Check that the name is spelled correctly and is in scope at this point.")
    )]
    VariableNotFound(String, #[label("undefined")] TokenRange),

    #[error("Type mismatch: expected {expected}, found {found}")]
    #[diagnostic(code(relon::eval::type_mismatch))]
    TypeMismatch {
        expected: String,
        found: String,
        #[label("expected {expected}, got {found}")]
        range: TokenRange,
    },

    #[error("Validation failed: {0}")]
    #[diagnostic(code(relon::eval::validation_failed))]
    ValidationError(String, #[label("validation failed here")] TokenRange),

    #[error("Division by zero")]
    #[diagnostic(
        code(relon::eval::division_by_zero),
        help("The right-hand operand of `/` or `%` evaluated to 0.")
    )]
    DivisionByZero(#[label("divisor is zero")] TokenRange),

    #[error("Function not found: {0}")]
    #[diagnostic(code(relon::eval::function_not_found))]
    FunctionNotFound(String, #[label("called here")] TokenRange),

    #[error("Circular reference detected: {}", format_chain(.cycle))]
    #[diagnostic(
        code(relon::eval::circular_reference),
        help("Each entry depends on a later one in the cycle. Break the loop or replace one of the references with a literal value.")
    )]
    CircularReference {
        /// Path segments that form the cycle, in declaration order.
        cycle: Vec<String>,
        #[label("triggers the cycle")]
        range: TokenRange,
    },

    #[error("Unsupported operator {0:?}")]
    #[diagnostic(code(relon::eval::unsupported_operator))]
    UnsupportedOperator(String, #[label("not supported here")] TokenRange),

    #[error("Invalid identifier: {0}")]
    #[diagnostic(
        code(relon::eval::invalid_identifier),
        help("Function/decorator names must start with a letter or underscore and contain only alphanumeric characters or underscores.")
    )]
    InvalidIdentifier(String, #[label("invalid identifier")] TokenRange),

    #[error("IO error: {0}")]
    #[diagnostic(code(relon::eval::io_error))]
    IoError(String),

    #[error("Module not found at path: {0}")]
    #[diagnostic(
        code(relon::eval::module_not_found),
        help("Check the path is relative to the importing file (or absolute) and that the file exists.")
    )]
    ModuleNotFound(String, #[label("import target missing")] miette::SourceSpan),

    #[error("Parse error in module {path}: {message}")]
    #[diagnostic(code(relon::eval::module_parse_error))]
    ModuleParseError {
        path: String,
        message: String,
        #[label("imported here")]
        range: miette::SourceSpan,
    },

    #[error("Circular import detected: {}", format_chain(.0))]
    #[diagnostic(
        code(relon::eval::circular_import),
        help("Two or more modules import each other. Restructure so the dependency is one-way.")
    )]
    CircularImport(
        Vec<String>,
        #[label("import that closes the cycle")] miette::SourceSpan,
    ),

    #[error("Numeric overflow")]
    #[diagnostic(code(relon::eval::numeric_overflow))]
    NumericOverflow(#[label("overflowed here")] TokenRange),

    #[error("Step limit exceeded ({limit} evaluation steps)")]
    #[diagnostic(
        code(relon::eval::step_limit_exceeded),
        help("The script ran longer than the configured `max_steps` budget. Raise `Capabilities::max_steps` or refactor recursive / iterative work.")
    )]
    StepLimitExceeded {
        limit: u64,
        #[label("budget exhausted here")]
        range: TokenRange,
    },

    #[error("Recursion limit exceeded ({limit} levels)")]
    #[diagnostic(
        code(relon::eval::recursion_limit_exceeded),
        help("A type-check or schema-validation pass nested deeper than the runtime's safety bound. Restructure the recursive type or value so it doesn't self-reference past this depth.")
    )]
    RecursionLimitExceeded {
        limit: usize,
        #[label("depth limit reached here")]
        range: TokenRange,
    },

    #[error("Value too large: {actual} elements exceeds limit of {limit}")]
    #[diagnostic(
        code(relon::eval::value_too_large),
        help("A list/dict grew past `Capabilities::max_value_elements`. Raise the limit or shrink the value.")
    )]
    ValueTooLarge {
        limit: usize,
        actual: usize,
        #[label("constructed here")]
        range: TokenRange,
    },

    #[error("Capability denied: native function `{name}` ({reason})")]
    #[diagnostic(
        code(relon::eval::capability_denied),
        help("This Context is sandboxed. Grant the capability declared on the fn's gate (e.g. `caps.reads_fs = true`) to permit it.")
    )]
    CapabilityDenied {
        name: String,
        reason: String,
        #[label("call rejected by sandbox")]
        range: TokenRange,
    },

    #[error("file has no `#main(...)` signature; cannot run as entry program")]
    #[diagnostic(
        code(relon::eval::no_main_signature),
        help(
            "Add `#main(Type arg, ...)` to declare the file as an entry program, or evaluate it as a static config via `eval_root` instead of `run_main`."
        )
    )]
    NoMainSignature {
        #[label("no #main here")]
        range: TokenRange,
    },

    #[error("missing argument `{name}` for `#main(...)`")]
    #[diagnostic(
        code(relon::eval::missing_main_arg),
        help("The host must push a value for every parameter declared by `#main(...)`.")
    )]
    MissingMainArg {
        name: String,
        #[label("expected here")]
        range: TokenRange,
    },

    #[error("unexpected argument `{name}`: not declared by `#main(...)`")]
    #[diagnostic(
        code(relon::eval::unexpected_main_arg),
        help("Only parameters listed in `#main(...)` may be pushed; remove the extra entry or add it to the signature.")
    )]
    UnexpectedMainArg {
        name: String,
        #[label("not in signature")]
        range: TokenRange,
    },

    #[error("type mismatch for `#main` arg `{name}`: expected {expected}, found {found}")]
    #[diagnostic(code(relon::eval::main_arg_type_mismatch))]
    MainArgTypeMismatch {
        name: String,
        expected: String,
        found: String,
        #[label("type mismatch")]
        range: TokenRange,
    },

    #[error("type mismatch for `#main` return value: expected {expected}, found {found}")]
    #[diagnostic(code(relon::eval::main_return_type_mismatch))]
    MainReturnTypeMismatch {
        expected: String,
        found: String,
        #[label("declared here")]
        range: TokenRange,
    },

    // -----------------------------------------------------------------
    // Wasm-AOT trap surface (Phase 7).
    //
    // These variants are produced exclusively by the wasm backend's
    // `WasmModule::translate_trap`. The tree-walker emits
    // `CapabilityDenied { name, reason, range }` / `ValueTooLarge {
    // limit, actual, range }`; the wasm path loses the names/limits
    // because the trap fires under a hot-path guard that only carries
    // a numeric `cap_bit` or buffer size. Keeping the two surfaces
    // distinct lets the Phase 8 facade route diagnostics correctly
    // without forcing the wasm backend to fabricate placeholder names.
    /// Phase 7: wasm trap fired because the granted-capabilities
    /// bitmap lacks the bit a host-fn call requires. The bit index
    /// matches the wasm module's `relon.host_fns` entry; the source
    /// range resolves to the `#native fn` call site via srcmap.
    #[error("Capability denied: wasm host-fn requires capability bit {cap_bit}")]
    #[diagnostic(
        code(relon::eval::wasm_capability_denied),
        help("The wasm module's `check_cap` prologue tripped because the host's `cap_grants` bitmap did not include this bit. Grant the matching capability before instantiation.")
    )]
    WasmCapabilityDenied {
        /// Bit index in the `relon_caps_avail` bitmap whose absence
        /// triggered the trap. Mirrors the `cap_bit` on the wasm
        /// module's host-fn entry.
        cap_bit: u32,
        #[label("capability check tripped here")]
        range: TokenRange,
    },

    /// Phase 7: wasm trap fired in the entry function's `out_cap`
    /// guard because the caller's output buffer is smaller than the
    /// return schema's fixed-area root size. The `needed` field is
    /// the minimum the wasm module expected; the actual `out_cap` is
    /// not preserved at the trap site (the guard fires before the
    /// runtime can capture it, so callers must reconstruct it from
    /// their own call args if they want a `got` value to report).
    #[error("output buffer too small: wasm entry expects at least {needed} bytes")]
    #[diagnostic(
        code(relon::eval::wasm_out_buf_too_small),
        help("Raise the `out_cap` you pass to `run_main` to at least the return schema's fixed-area root size (plus any tail-record overhead).")
    )]
    WasmOutBufTooSmall {
        /// Minimum bytes the wasm module's guard required.
        needed: u32,
        #[label("entry-function out_cap guard")]
        range: TokenRange,
    },

    /// Phase 7: wasm trap fired in the entry function's `in_len`
    /// guard because the caller's input buffer is smaller than the
    /// `#main` param schema's fixed-area root size. Mirrors
    /// `WasmOutBufTooSmall` on the input side.
    #[error("input buffer too small: wasm entry expects at least {needed} bytes")]
    #[diagnostic(
        code(relon::eval::wasm_in_buf_too_small),
        help("Make sure the `in_buf` you pass to `run_main` was populated by `BufferBuilder` against the same schema the wasm module was compiled for.")
    )]
    WasmInBufTooSmall {
        /// Minimum bytes the wasm module's guard required.
        needed: u32,
        #[label("entry-function in_len guard")]
        range: TokenRange,
    },

    /// Phase 7: wasm trap fired in a tail-record bounds check
    /// (`StoreField` of `String` / `List<Int>`, or a sub-record
    /// `AllocSubRecord`) because the value to be written wouldn't
    /// fit between the current `tail_cursor` and the caller's
    /// `out_cap`. The `kind` tag tells the host which shape ran
    /// over: `"String"`, `"ListInt"`, or `"Record"`.
    #[error("value too large: wasm tail-cursor overran `out_cap` while writing a {kind}")]
    #[diagnostic(
        code(relon::eval::wasm_value_too_large),
        help("The aggregate return ran past the caller's `out_cap`. Raise the buffer capacity or shrink the produced value.")
    )]
    WasmValueTooLarge {
        /// Static tag identifying the tail-record shape that overran.
        kind: &'static str,
        #[label("tail-cursor bounds check tripped here")]
        range: TokenRange,
    },

    /// Phase 7 placeholder: wasm step-limit / fuel exhaustion. The
    /// v1 AOT backend does not emit a step counter, but the variant
    /// is reserved so Phase 8+ can wire wasmtime's `OutOfFuel` trap
    /// without churning the enum surface.
    #[error("wasm step / fuel limit exhausted")]
    #[diagnostic(
        code(relon::eval::wasm_step_limit_exceeded),
        help("The wasm runtime stopped executing because the host's fuel budget hit zero.")
    )]
    WasmStepLimitExceeded {
        #[label("budget exhausted here")]
        range: Option<TokenRange>,
    },

    /// Phase 7 catch-all: a wasm trap that doesn't match any
    /// known Relon-emitted guard. Surfaces the wasmtime trap code
    /// stringified plus the module-absolute pc so a host can still
    /// produce a meaningful diagnostic for unexpected shapes
    /// (memory OOB, stack overflow, indirect-call type mismatch,
    /// ...). `range` is best-effort — `None` when the trap pc
    /// falls outside the srcmap's entry table.
    #[error("unclassified wasm trap `{trap_code}` at pc {pc:#x}")]
    #[diagnostic(
        code(relon::eval::wasm_trap_unclassified),
        help("The wasm runtime reported a trap shape this backend doesn't recognise. Inspect the trap_code and re-run with a debug build for more context.")
    )]
    WasmTrapUnclassified {
        /// Stringified wasmtime trap code (e.g. `"MemoryOutOfBounds"`).
        trap_code: String,
        /// Module-absolute byte offset of the trapping instruction,
        /// or `0` when the runtime didn't surface a pc.
        pc: u32,
        /// Source range — `Some` when the srcmap covers `pc`,
        /// `None` for stdlib / synthetic / out-of-range pcs.
        range: Option<TokenRange>,
    },

    /// Phase 8: the active backend cannot satisfy the requested
    /// `Evaluator` method. The wasm-AOT backend uses this to refuse
    /// `eval` / `eval_root` / `force_thunk` / `invoke_closure` because
    /// its AST is consumed at compile time and the runtime only knows
    /// how to drive the precompiled `run_main` entry. Host-side hooks
    /// that depend on lazy / first-class-closure semantics need to
    /// either switch to the tree-walker or be reformulated.
    #[error("operation not supported by this backend: {reason}")]
    #[diagnostic(
        code(relon::eval::unsupported),
        help("This backend lacks the runtime structures the operation needs. Switch to the tree-walking backend, or restrict the call to `run_main`.")
    )]
    Unsupported {
        /// Human-readable explanation of why the backend cannot
        /// honour the call. Free-form so each backend can describe
        /// its own constraint (e.g. "wasm-aot has no AST at runtime").
        reason: String,
    },
}
