//! Value types moved to `relon_eval_api::value`. This module re-exports
//! them so existing intra-crate `use crate::value::Value;` imports keep
//! working without touching every caller.

pub use relon_eval_api::value::{
    ClosureData, EnumSchemaData, SchemaData, SchemaField, Value, ValueDict,
};
