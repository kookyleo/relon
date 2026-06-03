//! `Op`-family: schema-method dispatch.
//!
//! Placeholder for the schema surface — `LoadSchemaPtr` and the
//! schema-method dispatch ops are still routed to the `unsupported`
//! arm in `super::lower_op`. Phase 0b fills the `emit_*` methods here
//! as `impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp>` blocks; the central
//! dispatch then delegates to them.
//!
//! Intentionally empty today (no behavior change in Phase 0a).
