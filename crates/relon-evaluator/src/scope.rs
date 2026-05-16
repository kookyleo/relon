//! Scope / Thunk types moved to `relon_eval_api::scope`. This module
//! re-exports them so existing intra-crate `use crate::scope::Scope;`
//! imports keep working.

pub use relon_eval_api::scope::{ListContext, Locals, RootRef, Scope, Thunk, Thunks};
