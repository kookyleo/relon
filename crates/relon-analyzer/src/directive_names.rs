//! Canonical directive-name string constants.
//!
//! Re-exports from `relon-parser::directive` so dispatch by name uses
//! one source of truth. Adding a new directive means adding it once in
//! the parser; analyzer and evaluator pick it up automatically.

#![allow(dead_code)]

pub(crate) use relon_parser::directive::{
    BRAND, DEFAULT, ERROR, EXPECT, EXTEND, IMPORT, MAIN, MSG, SCHEMA, STRICT,
};
