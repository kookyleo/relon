//! Iter protocol constants.
//!
//! `Iter` values are encoded as branded dicts carrying `_kind` /
//! `_source` / `_id` fields. The brand string + field names + the
//! three kind discriminants are referenced from both `stdlib.rs`
//! (`make_iter_value`, `IterNext`, `IterFromList` / `String` / `Dict`)
//! and `eval.rs` (`materialize_iterable`). Centralising them here
//! gives every site one source of truth — adding a new iter source
//! kind (or renaming a field) no longer requires a cross-file
//! find-and-replace.

/// Schema brand stamped onto every `Iter` dict.
pub const BRAND: &str = "Iter";

/// Dict field carrying the driver dispatch tag (one of [`KIND_LIST`] /
/// [`KIND_STRING`] / [`KIND_DICT_ENTRIES`]).
pub const FIELD_KIND: &str = "_kind";
/// Dict field holding the underlying source value the cursor walks.
pub const FIELD_SOURCE: &str = "_source";
/// Dict field carrying the per-construction cursor id used to key into
/// `Context::iter_cursors`.
pub const FIELD_ID: &str = "_id";

/// `_kind` value for `List<T>.iter()` — element-by-element walk.
pub const KIND_LIST: &str = "list";
/// `_kind` value for `String.iter()` — one-codepoint-per-step walk.
pub const KIND_STRING: &str = "string";
/// `_kind` value for `Dict<K, V>.iter()` — sorted `(K, V)` pair walk.
pub const KIND_DICT_ENTRIES: &str = "dict_entries";
