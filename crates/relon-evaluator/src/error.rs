//! `RuntimeError` moved to `relon_eval_api::error`. Re-exported here so
//! existing intra-crate `use crate::error::RuntimeError;` keeps working.

pub use relon_eval_api::error::RuntimeError;
