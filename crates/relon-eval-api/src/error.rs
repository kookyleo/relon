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

    /// Phase 4.c-2: an index / range operation walked off the end of
    /// a String / List receiver. Both backends share this variant —
    /// the tree-walker raises it from `xs[i]` style accessors, the
    /// wasm AOT path raises it from `substring` / similar stdlib
    /// builders when the caller-supplied bounds exceed the
    /// receiver's length.
    #[error("Index out of bounds")]
    #[diagnostic(
        code(relon::eval::index_out_of_bounds),
        help("Inspect the receiver's length before indexing, or clamp the offset / length arguments so the slice stays inside the value.")
    )]
    IndexOutOfBounds {
        #[label("index walked past the receiver length")]
        range: TokenRange,
    },

    /// Phase 4.c-2: a reducer that requires at least one element
    /// (`list_int_max`, future `head` / `last`, ...) was called on
    /// an empty list. Carries the call-site source range so the
    /// diagnostic points at the offending expression rather than at
    /// the stdlib body itself.
    #[error("Operation on empty list has no defined result")]
    #[diagnostic(
        code(relon::eval::empty_list),
        help("Reducers like `list_int_max` need at least one element. Check the list isn't empty before calling, or supply an explicit fallback value.")
    )]
    EmptyList {
        #[label("called on an empty list here")]
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

    /// Phase 4.c-2: wasm trap fired in a stdlib bounds check
    /// (`substring(s, start, len)` and similar) because the requested
    /// slice walks past the receiver's end. Mirrors
    /// [`Self::IndexOutOfBounds`] but is emitted from the wasm
    /// translate_trap path so the wasm srcmap can decorate the call
    /// site.
    #[error("wasm trap: index walked past receiver length")]
    #[diagnostic(
        code(relon::eval::wasm_index_out_of_bounds),
        help(
            "Check the bounds you pass to substring / slice stdlib helpers before invoking them."
        )
    )]
    WasmIndexOutOfBounds {
        #[label("index out of bounds here")]
        range: TokenRange,
    },

    /// Phase 4.c-2: wasm trap fired in a reducer body that requires
    /// at least one element (`list_int_max`) when invoked on an
    /// empty list. Mirrors [`Self::EmptyList`] on the wasm side.
    #[error("wasm trap: reducer invoked on an empty list")]
    #[diagnostic(
        code(relon::eval::wasm_empty_list),
        help("Guard the reducer call with a non-emptiness check, or pick a stdlib variant that accepts an explicit fallback.")
    )]
    WasmEmptyList {
        #[label("empty list reducer tripped here")]
        range: TokenRange,
    },

    /// v3+ a-4: wasm trap fired inside the Unicode-aware `upper` /
    /// `lower` stdlib bodies because the receiver String contained a
    /// byte sequence that does not decode as valid UTF-8 (a truncated
    /// trailing continuation byte, a lone continuation byte, or a
    /// leading byte with an illegal high-bit pattern).
    ///
    /// In production the host SDK's `BufferBuilder::write_string`
    /// rejects non-UTF-8 inputs at write time so this trap is mostly
    /// a defensive surface — it can still fire when a host hand-
    /// crafts a String record into linear memory and skips the
    /// builder. Routing through a dedicated variant lets the host
    /// distinguish "invalid encoding" from "ran out of memory" or
    /// "tail-cursor overrun".
    #[error("wasm trap: stdlib walker hit invalid UTF-8 in the input string")]
    #[diagnostic(
        code(relon::eval::wasm_invalid_utf8),
        help("Use `BufferBuilder::write_string` (or another validated path) to populate String fields the wasm module reads — that path rejects ill-formed UTF-8 before the trap can fire.")
    )]
    WasmInvalidUtf8 {
        #[label("invalid UTF-8 byte sequence")]
        range: TokenRange,
    },

    /// Phase 4.c-1: wasm trap fired because the scratch bump allocator
    /// (`Op::AllocScratch` / `Op::AllocScratchDyn`) would have pushed
    /// the `relon_scratch_cursor` global past the end of wasm linear
    /// memory. The `needed` field is the byte count the failing alloc
    /// requested when the size was statically known; the dynamic-size
    /// variant reports `0` because the operand-stack size is consumed
    /// by the bump check before the trap fires.
    #[error("wasm scratch heap exhausted (alloc requested {needed} bytes)")]
    #[diagnostic(
        code(relon::eval::wasm_scratch_oom),
        help("The wasm module's internal bump scratch overran linear memory. Pass a larger `out_cap` to leave room for stdlib scratch, or split the request into smaller chunks.")
    )]
    WasmScratchOOM {
        /// Byte count the trapping alloc requested. `0` when the
        /// size came from the operand stack (dynamic variant).
        needed: u32,
        #[label("scratch bump tripped here")]
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

    /// v3+ a-3: remote `#import "https://..."` resolved an URL but the
    /// HTTP fetch (DNS / connect / TLS / non-2xx status / body read)
    /// failed. The payload is boxed so the variant does not bloat the
    /// `RuntimeError` enum past clippy's `result_large_err` threshold —
    /// callers should use the `url()` / `cause()` accessors below, or
    /// destructure `*payload`.
    #[error("remote import {}: {}", payload.url, payload.cause)]
    #[diagnostic(
        code(relon::eval::remote_import_failed),
        help("The host could not retrieve the remote module. Check connectivity, the URL, and that the server returns a 2xx response with a Relon source body.")
    )]
    RemoteImportFailed {
        payload: Box<RemoteImportFailure>,
        #[label("remote import failed")]
        range: TokenRange,
    },

    /// v3+ a-3: remote `#import "https://..."` was rejected before the
    /// fetch ran because the active sandbox forbids network egress
    /// (no `--trust` / no `Capabilities::network`).
    #[error("remote import {} denied: {}", payload.url, payload.reason)]
    #[diagnostic(
        code(relon::eval::remote_import_denied),
        help("Remote `#import` is a network operation. Run the host with `--trust` (CLI) or grant `Capabilities::network` to allow it.")
    )]
    RemoteImportDenied {
        payload: Box<RemoteImportDenial>,
        #[label("remote import rejected by sandbox")]
        range: TokenRange,
    },

    /// v3+ a-3: an explicit integrity hash was supplied alongside a
    /// remote `#import`, and the fetched body's sha256 did not match.
    /// The pinning syntax itself is **not** wired in this phase, but
    /// the variant ships so future syntax work (or an out-of-band
    /// lockfile) can reuse the error surface without churning the
    /// enum.
    #[error(
        "remote import {} hash mismatch: expected {}, got {}",
        payload.url,
        payload.expected,
        payload.got
    )]
    #[diagnostic(
        code(relon::eval::remote_import_hash_mismatch),
        help("The remote source's sha256 differs from the pinned hash. Either update the pin or refuse to load the module.")
    )]
    RemoteImportHashMismatch {
        payload: Box<RemoteImportHashMismatchDetail>,
        #[label("hash mismatch on remote import")]
        range: TokenRange,
    },

    /// review-improvement-174 (v3++ b-2 fix): the evaluator's `#import`
    /// path computed the loaded module body's digest and it did not match
    /// the inline `sha256:"..."` integrity pin written on the directive.
    ///
    /// Distinct from [`Self::RemoteImportHashMismatch`] so operators can
    /// tell apart "remote fetch produced an unexpected body" (caught by
    /// `RemoteHttpResolver` / analyzer) from "evaluator was handed a
    /// pre-resolved module body that disagrees with its pin" — the latter
    /// is the analyzer-bypass attack vector this fix closes.
    #[error(
        "import {} hash mismatch: expected {}:{}, got {}",
        payload.path,
        payload.algorithm,
        payload.expected,
        payload.got
    )]
    #[diagnostic(
        code(relon::eval::import_hash_mismatch),
        help("The module body the evaluator loaded does not match the inline integrity pin on this `#import`. Either update the pin to the new digest or refuse to trust the source.")
    )]
    ImportHashMismatch {
        payload: Box<ImportHashMismatchDetail>,
        #[label("import body does not match pinned digest")]
        range: TokenRange,
    },

    /// review-improvement-174: the inline pin on a `#import` carried an
    /// algorithm identifier (`<algo>:"..."`) the evaluator does not know
    /// how to compute. The analyzer surfaces the same condition as a
    /// `WorkspaceDiagnostic::ImportHashUnknownAlgorithm`; this variant
    /// mirrors it for the analyzer-bypass path so the evaluator never
    /// silently treats an unknown algorithm as "no pin".
    #[error("import {path} pinned with unsupported hash algorithm `{algorithm}`")]
    #[diagnostic(
        code(relon::eval::import_hash_unknown_algorithm),
        help("Use a supported algorithm (currently `sha256:`). The evaluator refuses to load an `#import` it cannot verify against the pin.")
    )]
    ImportHashUnknownAlgorithm {
        path: String,
        algorithm: String,
        #[label("unsupported integrity algorithm")]
        range: TokenRange,
    },

    /// review-improvement-174: the inline pin hex was malformed (wrong
    /// length, non-hex character). Mirrors the analyzer's
    /// `WorkspaceDiagnostic::ImportHashInvalidHex` for the
    /// evaluator-direct path; a malformed pin is rejected fail-closed
    /// because we cannot compare against gibberish.
    #[error(
        "import {path} pinned with invalid {algorithm} hex (expected {expected_len} chars, got {got_len})"
    )]
    #[diagnostic(
        code(relon::eval::import_hash_invalid_hex),
        help("The pin's hex digest is not the expected length or contains non-hex characters. Re-encode the digest as lowercase hex.")
    )]
    ImportHashInvalidHex {
        path: String,
        algorithm: String,
        expected_len: usize,
        got_len: usize,
        #[label("invalid integrity hex")]
        range: TokenRange,
    },
}

/// Boxed payload for [`RuntimeError::RemoteImportFailed`]. Holds the
/// URL the host attempted to fetch plus a free-form cause string so
/// the per-host fetch error type does not leak into the enum surface.
#[derive(Debug, Clone)]
pub struct RemoteImportFailure {
    pub url: String,
    pub cause: String,
}

/// Boxed payload for [`RuntimeError::RemoteImportDenied`]. Holds the
/// URL the script attempted to import plus the human-readable reason
/// the sandbox refused it.
#[derive(Debug, Clone)]
pub struct RemoteImportDenial {
    pub url: String,
    pub reason: String,
}

/// Boxed payload for [`RuntimeError::RemoteImportHashMismatch`]. The
/// hash-pinning syntax is not wired yet (see the variant doc), but
/// the type ships so the eventual lockfile / inline-pin work can
/// produce it without churning the error enum's layout.
#[derive(Debug, Clone)]
pub struct RemoteImportHashMismatchDetail {
    pub url: String,
    pub expected: String,
    pub got: String,
}

/// Boxed payload for [`RuntimeError::ImportHashMismatch`]. Carries the
/// raw `#import` path, the algorithm name, and the expected / actual
/// digests so error rendering can surface enough context for the
/// operator to decide whether to update the pin or refuse the load.
#[derive(Debug, Clone)]
pub struct ImportHashMismatchDetail {
    /// `#import "..."` path as written in source (may be a local path,
    /// `std/...`, or a `https://` URL — the integrity check is
    /// path-agnostic so the analyzer-bypass attack vector cannot find
    /// a path shape that skips verification).
    pub path: String,
    /// Algorithm identifier as it appears in the pin (e.g. `sha256`).
    pub algorithm: String,
    /// Lower-case hex digest the pin asserted.
    pub expected: String,
    /// Lower-case hex digest the evaluator computed over the loaded
    /// module body.
    pub got: String,
}
