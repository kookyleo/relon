//! Canonical decorator / directive name string constants.
//!
//! Centralized so a typo in one site can't silently break dispatch — the
//! evaluator's plugin registry, the analyzer's special-case checks, and
//! any host code referring to built-in attributes all funnel through
//! these constants.
//!
//! After the sigil split (batch 3):
//!
//! * **Directives** (`#name`) cover declaration / structure / metadata:
//!   `#schema`, `#import`, `#default`, `#expect`, `#msg`, `#error`,
//!   `#brand`, `#private`, `#main`. Host-registered only.
//! * **Decorators** (`@name(...)`) cover value transforms:
//!   `@value` (the only built-in) plus user-defined `@f` whose name
//!   resolves to a callable in scope.

#![allow(dead_code)]

// `#`-directive names ----------------------------------------------------

pub const SCHEMA: &str = "schema";
pub const IMPORT: &str = "import";
pub const EXPECT: &str = "expect";
pub const DEFAULT: &str = "default";
pub const MSG: &str = "msg";
pub const ERROR: &str = "error";
pub const BRAND: &str = "brand";
/// Marks a dict entry as not externally visible: it lives in the
/// owning dict's local scope (so siblings can reference it) but never
/// appears in the dict's `map` — which means it cannot be reached
/// through a stored Value, gets skipped by `#import * from "..."`,
/// causes `lib.private` to fail with `VariableNotFound`, and is hidden
/// from any [`crate::Projector`] (including the default JSON one).
pub const PRIVATE: &str = "private";
pub const MAIN: &str = "main";

// `@`-decorator names ----------------------------------------------------

pub const VALUE: &str = "value";
