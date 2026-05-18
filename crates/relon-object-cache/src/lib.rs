//! `relon-object-cache` — on-disk + in-memory cache layer for the
//! cranelift-object `.o` artefacts produced by `relon-codegen-native`
//! during the v5-gamma cold-start pipeline.
//!
//! Crate skeleton only — the seven concrete modules (`error`,
//! `integrity`, `storage`, `hmac`, `loader`, plus their test files)
//! land in follow-up commits per the v5-gamma milestone plan in
//! `docs/internal/v5-gamma-cranelift-object-cache-design.md`.

#![deny(unsafe_op_in_unsafe_fn)]
