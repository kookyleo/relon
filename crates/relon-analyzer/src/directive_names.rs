//! Canonical directive-name string constants.
//!
//! Mirrors `relon-evaluator`'s copy. We keep a private duplicate here to
//! avoid a parser/evaluator dep direction conflict; both lists must stay
//! in sync — a typo in either side silently breaks dispatch.

#![allow(dead_code)]

pub(crate) const SCHEMA: &str = "schema";
pub(crate) const IMPORT: &str = "import";
pub(crate) const EXPECT: &str = "expect";
pub(crate) const DEFAULT: &str = "default";
pub(crate) const MSG: &str = "msg";
pub(crate) const ERROR: &str = "error";
pub(crate) const BRAND: &str = "brand";
pub(crate) const PRIVATE: &str = "private";
pub(crate) const MAIN: &str = "main";
