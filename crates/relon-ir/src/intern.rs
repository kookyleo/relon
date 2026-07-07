//! Compile-time const-literal intern table shared across every
//! `LowerCtx` in a module (#151 String Tier 2a).
//!
//! ## Problem this solves
//!
//! Before #151 the lowering pass kept four `next_*_idx` counters on
//! each `crate::lowering::LowerCtx` and minted a fresh idx for every
//! literal it walked past — even when two `Op::ConstString` shared the
//! same bytes. Two downstream consequences:
//!
//! 1. **No dedup.** A `["a", "a", "a"]` list-literal source produces
//!    three distinct `Op::ConstString { idx, value: "a" }` records.
//!    The const-pool layout walks each record in declaration order and
//!    appends `[len:u32 LE][bytes]` to the pool blob per unique idx —
//!    so the pool ended up with three identical `"a"` records (and
//!    three distinct offsets that downstream dict-key compares treated
//!    as different addresses).
//! 2. **Latent cross-function idx collision.** `LowerCtx::new` /
//!    `new_method` reset every counter to `0`. The const-pool stores
//!    `idx -> offset` in a single per-module `HashMap<u32, u32>`, so
//!    if a schema method *and* the entry body both emitted
//!    `Op::ConstString { idx: 0 }` the second insert silently no-op'd
//!    (the first writer's offset wins). The method's `idx 0` then
//!    resolved to the entry body's `"a"` string — a real bug latent
//!    only because no shipped corpus exercised string literals from
//!    multiple funcs in one module.
//!
//! ## Design
//!
//! [`ConstInternTables`] holds one [`StringInternTable`] plus a
//! shared id-allocator per `ConstList*` variant. The table is built
//! once per `crate::lowering::lower_workspace_*` call and threaded
//! through every `LowerCtx::new_with_intern` ctor so all funcs in the
//! same `Module` share one idx space. `Rc<RefCell<...>>` keeps the
//! API ergonomic for the recursive walker (nested closures /
//! schema-method bodies need transient `&mut` access through borrows
//! that may overlap with the outer ctx's existing borrows).
//!
//! ## ConstString intern surface
//!
//! [`StringInternTable::intern`] looks the bytes up in
//! `by_bytes: HashMap<String, u32>`. Hit → return the existing idx
//! (the second / Nth `Op::ConstString { value: "a" }` reuses the
//! first emitter's idx). Miss → allocate the next sequential idx,
//! insert, and return.
//!
//! ## List<*> id-allocator surface
//!
//! Lists are NOT byte-deduplicated yet — the IR carries the elements
//! inline on the op and the producer-side const-pool walker uses the
//! idx as the dedup key. We thread the counter through the same
//! shared table so the module-wide idx-uniqueness invariant the const
//! pool relies on holds for every list variant too (fixing the
//! latent collision risk for `Op::ConstList*` even though we don't
//! intern their payloads).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Module-wide intern table for string literals plus per-variant idx
/// allocators for `Op::ConstList*`. Shared (via `Rc<RefCell<...>>`)
/// across every `LowerCtx` lowered for the same `Module`.
#[derive(Debug, Default)]
pub struct ConstInternTables {
    pub strings: StringInternTable,
    pub list_int_next: u32,
    pub list_float_next: u32,
    pub list_bool_next: u32,
    pub list_string_next: u32,
    /// W5-P1: next idx to mint for an `Op::ConstDict` record.
    pub dict_next: u32,
}

impl ConstInternTables {
    /// Allocate a new shared intern table. The caller stores the
    /// returned handle on every `LowerCtx` it spawns for the same
    /// `Module`.
    pub fn shared() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::default()))
    }

    /// Mint a fresh module-unique idx for an `Op::ConstListInt` record.
    pub fn alloc_list_int_idx(&mut self) -> u32 {
        let idx = self.list_int_next;
        self.list_int_next += 1;
        idx
    }
    /// Mint a fresh module-unique idx for an `Op::ConstListFloat`.
    pub fn alloc_list_float_idx(&mut self) -> u32 {
        let idx = self.list_float_next;
        self.list_float_next += 1;
        idx
    }
    /// Mint a fresh module-unique idx for an `Op::ConstListBool`.
    pub fn alloc_list_bool_idx(&mut self) -> u32 {
        let idx = self.list_bool_next;
        self.list_bool_next += 1;
        idx
    }
    /// Mint a fresh module-unique idx for an `Op::ConstListString`.
    pub fn alloc_list_string_idx(&mut self) -> u32 {
        let idx = self.list_string_next;
        self.list_string_next += 1;
        idx
    }
    /// W5-P1: mint a fresh module-unique idx for an `Op::ConstDict`.
    pub fn alloc_dict_idx(&mut self) -> u32 {
        let idx = self.dict_next;
        self.dict_next += 1;
        idx
    }
}

/// Byte-keyed dedup table for `Op::ConstString` literals.
///
/// Two `Op::ConstString { value }` with the same `value` bytes resolve
/// to the same idx, so the downstream const-pool walker emits a single
/// `[len][payload]` record and every reference materialises the same
/// `i32.const <offset>` at codegen time.
#[derive(Debug, Default)]
pub struct StringInternTable {
    /// `bytes -> assigned idx`. Owned `String` so the key lives as
    /// long as the table without forcing every caller to keep the
    /// source `Op::ConstString { value }` around.
    by_bytes: HashMap<String, u32>,
    /// Next idx to mint on a miss. Equals `by_bytes.len()` at all
    /// times — kept explicit so the allocator stays trivial to read.
    next: u32,
}

impl StringInternTable {
    /// Look up `value` in the dedup table. On hit return the existing
    /// idx; on miss assign the next sequential idx and store it.
    /// Cloning the bytes for the key is unavoidable since the source
    /// `Op::ConstString` owns the only copy — the cost is paid once
    /// per unique literal at lowering time and amortises against the
    /// downstream const-pool record + every code emit that resolves
    /// the same idx to the cached offset.
    pub fn intern(&mut self, value: &str) -> u32 {
        if let Some(&idx) = self.by_bytes.get(value) {
            return idx;
        }
        let idx = self.next;
        self.next += 1;
        self.by_bytes.insert(value.to_owned(), idx);
        idx
    }

    /// Number of distinct string literals interned so far. Test-only
    /// surface used by the dedup invariant tests.
    pub fn unique_count(&self) -> usize {
        self.by_bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two intern calls with the same bytes resolve to the same idx
    /// (the core dedup contract).
    #[test]
    fn intern_same_bytes_returns_same_idx() {
        let mut table = StringInternTable::default();
        let a = table.intern("foo");
        let b = table.intern("foo");
        assert_eq!(a, b);
        assert_eq!(table.unique_count(), 1);
    }

    /// Distinct bytes get distinct sequential idxs.
    #[test]
    fn intern_distinct_bytes_get_sequential_idxs() {
        let mut table = StringInternTable::default();
        assert_eq!(table.intern("a"), 0);
        assert_eq!(table.intern("b"), 1);
        assert_eq!(table.intern("a"), 0); // repeat dedup
        assert_eq!(table.intern("c"), 2);
        assert_eq!(table.unique_count(), 3);
    }

    /// Empty-string literals dedup like any other byte sequence.
    #[test]
    fn empty_string_is_a_valid_intern_key() {
        let mut table = StringInternTable::default();
        let a = table.intern("");
        let b = table.intern("");
        assert_eq!(a, b);
        assert_eq!(table.unique_count(), 1);
    }

    /// `ConstInternTables` per-variant list allocators mint sequential
    /// idxs independent of each other (List<Int> idx 0 ≠ List<Bool>
    /// idx 0 at the IR level, but the const pool keys lists by
    /// (variant, idx) so reusing 0 across variants is safe).
    #[test]
    fn list_id_allocators_are_per_variant_and_sequential() {
        let mut t = ConstInternTables::default();
        assert_eq!(t.alloc_list_int_idx(), 0);
        assert_eq!(t.alloc_list_int_idx(), 1);
        assert_eq!(t.alloc_list_float_idx(), 0);
        assert_eq!(t.alloc_list_bool_idx(), 0);
        assert_eq!(t.alloc_list_string_idx(), 0);
        assert_eq!(t.alloc_list_int_idx(), 2);
    }
}
