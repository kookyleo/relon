//! Canonical decorator / directive name string constants.
//!
//! Directive names re-export from `relon-parser::directive` (the single
//! source of truth used by the parser's `DIRECTIVE_SHAPES` table).
//! `@`-decorator names live here because no other crate needs them.
//!
//! * **Directives** (`#name`) cover declaration / structure / metadata:
//!   `#schema`, `#import`, `#default`, `#expect`, `#msg`, `#error`,
//!   `#brand`, `#private`, `#main`. Host-registered only.
//! * **Decorators** (`@name(...)`) cover value transforms:
//!   `@value` (the only built-in) plus user-defined `@f` whose name
//!   resolves to a callable in scope.

#![allow(dead_code)]

pub use relon_parser::directive::{
    BRAND, DEFAULT, ERROR, EXPECT, IMPORT, MAIN, MSG, PRIVATE, SCHEMA,
};

// `@`-decorator names ----------------------------------------------------

pub const VALUE: &str = "value";
