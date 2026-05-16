//! `DecoratorPlugin` / `PreEvalOutcome` moved to
//! `relon_eval_api::decorator`. Re-exported here so existing intra-crate
//! `use crate::decorator::DecoratorPlugin;` keeps working.

pub use relon_eval_api::decorator::{DecoratorPlugin, PreEvalOutcome};
