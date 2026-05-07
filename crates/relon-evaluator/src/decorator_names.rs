//! Canonical decorator-name string constants.
//!
//! Centralized so a typo in one site can't silently break dispatch — the
//! evaluator's plugin registry, the analyzer's special-case checks, and
//! any host code referring to built-in decorators all funnel through
//! these constants.

pub const LIBRARY: &str = "library";
pub const SCHEMA: &str = "schema";
pub const IMPORT: &str = "import";
pub const EXPECT: &str = "expect";
pub const DEFAULT: &str = "default";
pub const VALUE: &str = "value";
pub const MSG: &str = "msg";
pub const ERROR: &str = "error";
pub const BRAND: &str = "brand";
/// Marks a root-level dict field as the file's input contract: its
/// body is a schema describing the host-pushed `input` tree (see the
/// reserved name `input` and `Context::with_input`). Implies `@schema`
/// semantics — the field's value becomes a `Value::Schema`, not data.
/// At most one `@input` per file.
pub const INPUT: &str = "input";
/// Marks a dict entry as not externally visible: it lives in the
/// owning dict's local scope (so siblings can reference it) but never
/// appears in the dict's `map` — which means it cannot be reached
/// through a stored Value, gets skipped by `@import(..., spread=true)`,
/// causes `lib.private` to fail with `VariableNotFound`, and is hidden
/// from any [`crate::Projector`] (including the default JSON one).
pub const PRIVATE: &str = "private";
