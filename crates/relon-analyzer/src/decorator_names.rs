//! Canonical decorator-name string constants.
//!
//! After the sigil split (batch 3), only host-registered value-transform
//! decorators live here. Structural / declarative attributes live in
//! [`crate::directive_names`].
//!
//! Mirrors `relon-evaluator`'s copy. We keep a private duplicate here to
//! avoid a parser/evaluator dep direction conflict; both lists must stay
//! in sync — a typo in either side silently breaks dispatch.

#![allow(dead_code)]

pub(crate) const VALUE: &str = "value";
