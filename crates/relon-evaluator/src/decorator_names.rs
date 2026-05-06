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
