//! `Op`-family: Unicode table-address ops.
//!
//! Placeholder for the Unicode surface — the `*TableAddr` ops
//! (`CaseFoldTableAddr`, `CombiningMarkRangesAddr`,
//! `WhitespaceRangesAddr`, `DecompTableAddr`, `CccTableAddr`,
//! `CompositionTableAddr`, `FullCaseFoldTableAddr`, `CasedRangesAddr`,
//! `CaseIgnorableRangesAddr`, `TurkishCaseFoldTableAddr`) are still
//! routed to the `unsupported` arm in `super::lower_op`. Phase 0b fills
//! the `emit_*` methods here as `impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp>`
//! blocks; the central dispatch then delegates to them.
//!
//! Intentionally empty today (no behavior change in Phase 0a).
