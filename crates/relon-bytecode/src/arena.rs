//! M2-B phase 4b — handle-based memory model for list / dict / string
//! values inside the bytecode VM.
//!
//! ## Rationale
//!
//! The operand-stack slot stays `u64` (see [`crate::vm::VmValue`]) so
//! the dispatch loop's arith / cmp / control-flow arms don't pay any
//! tagged-enum overhead. Composite values (lists, dicts, strings)
//! that won't fit in a `u64` live in per-type arenas keyed by a `u32`
//! handle the slot carries. The BcOp variant carries the type
//! discrimination — `BcOp::ListGetInt` knows it indexes the list
//! arena, `BcOp::DictLookupStr` knows it consults the dict arena —
//! so the slot itself stays untyped.
//!
//! ## Lifetime
//!
//! All three arenas are owned by the [`crate::vm::BytecodeVm`]
//! invocation. They allocate monotonically (no slot reuse, no GC) and
//! drop with the `BcRunOutcome` at `invoke` exit. Handles are
//! VM-local — they must not escape across `invoke` boundaries; any
//! host-fn return value that wants to outlive the call has to be
//! materialised back into a [`relon_eval_api::Value`] before the
//! arenas drop (see `vm::encode_value_for_ret` for the scalar lane;
//! phase 4b-continuation lands the list / dict / string lanes).
//!
//! ## Cost model
//!
//! - `alloc`: amortised O(1) `Vec::push`; first-touch arena allocates
//!   a backing `Vec` lazily.
//! - `get`: O(1) indexed read; returns a `&Arc<T>` so the caller pays
//!   refcount cost only when they `clone()`.
//! - `clone_handle`: O(1) refcount bump — no deep copy.
//!
//! Phase 4b scaffold ships [`ListArena`] + the two companion arenas;
//! only `ListArena` has callers as of this commit. The other two are
//! parked here so the phase-4b-continuation surface (strings + dicts)
//! drops in without re-litigating the arena layout.
//!
//! ## Out-of-scope (phase 4c+)
//!
//! - Slot reuse / freelist. Wait until a benchmark shows allocator
//!   pressure.
//! - Layout-sharing with the trace-JIT arena. The handle-based model
//!   is internal-only; the bridge lands in phase 4c when the
//!   trace-JIT recorder gets wired to bytecode.
//! - `Send` / `Sync`. The bytecode VM is single-threaded; we'll add
//!   the trait bounds when (if) the trace-JIT bridge needs them.

use std::sync::Arc;

use thiserror::Error;

/// Opaque handle into one of the bytecode VM's per-type arenas. The
/// numeric value is the slot index — exposed as a transparent `u32`
/// so the dispatch path can stash it in the operand-stack `u64` slot
/// without an extra wrapper allocation.
///
/// Handles are **not** type-tagged: callers learn the type from the
/// [`crate::op::BcOp`] variant that consumed / produced the slot. A
/// handle minted by [`ListArena::alloc`] passed to [`StringArena::get`]
/// is a compiler bug; the arena will either return an unrelated entry
/// or trip [`ArenaError::OutOfRange`] — but the type confusion itself
/// is on the BcOp lowering, not the arena.
pub type Handle = u32;

/// One slot in the list arena. Wrapped in [`Arc`] so refcount-only
/// clones suffice for the common "list flows through an op without
/// mutation" pattern. `Vec<u64>` mirrors the operand-stack slot shape
/// — list elements travel through the same i64 / f64-via-bits / bool
/// lane the dispatch loop uses for scalars.
///
/// Phase 4b only models type-uniform lists. A heterogeneous list
/// surfaces during host-fn lift as
/// [`crate::vm::BcVmError::HostFnReturnTypeMismatch`].
type ListSlot = Arc<Vec<u64>>;

/// One slot in the string arena. Bytecode VM string handling is
/// code-point-counted (matches tree-walker's `String::chars().count()`
/// semantics) but the slot itself is byte-shaped; `StrLen` walks the
/// UTF-8 boundaries on demand. The slot uses `Arc<str>` so refcount
/// clones suffice for the common "string flows through an op without
/// mutation" pattern.
type StringSlot = Arc<str>;

/// One slot in the dict arena. Phase 4b only models string-keyed
/// dicts (matches the IR `Op::DictGetByStringKey` surface); the value
/// slot is the operand-stack `u64` shape so it carries int / bool /
/// f64-via-bits uniformly with the rest of the dispatch lane.
///
/// Storage is a `Vec<(Arc<str>, u64)>` rather than a `HashMap` so
/// allocation cost stays bounded for the small-dict workloads the
/// bytecode VM handles; phase 4c (the trace-JIT bridge) can revisit
/// the storage shape if benchmarks show lookup cost dominating.
type DictSlot = Arc<Vec<(Arc<str>, u64)>>;

/// Arena-side error envelope. The dispatch loop lifts these into the
/// matching [`crate::vm::BcVmError`] variant — `OutOfRange` becomes
/// `IndexOutOfBounds`; the others stay arena-side because they
/// indicate compiler bugs (BcOp emitted a handle the arena never
/// minted), not runtime traps.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ArenaError {
    /// The supplied handle is past the arena's high-water mark.
    /// Indicates either a compiler bug (BcOp emitted a handle the
    /// arena never minted) or a partial-resume / trace-JIT bridge
    /// state mismatch (handle from a stale VM invocation leaked into
    /// a fresh one).
    #[error("arena handle {handle} out of range (arena has {len} slots)")]
    OutOfRange {
        /// The offending handle.
        handle: Handle,
        /// Arena length at the time of the failed access.
        len: usize,
    },
    /// Index into a list / dict slot is past the slot's element
    /// count. Surfaces as `BcVmError::IndexOutOfBounds` at the
    /// dispatch boundary; carried separately here so the
    /// "compiler-bug OutOfRange" path can stay distinct.
    #[error("element index {index} out of range (slot has {len} elements)")]
    ElementOutOfRange {
        /// The offending index.
        index: usize,
        /// Length of the slot at the time of the failed access.
        len: usize,
    },
}

/// Per-VM arena holding [`Vec<u64>`]-shaped list slots.
///
/// Allocation is monotonic — `alloc` pushes a new slot and returns
/// the slot index as the handle. No slot reuse / freelist; the arena
/// drops with the VM at `invoke` exit so the bookkeeping cost stays
/// out of the hot path.
#[derive(Debug, Default, Clone)]
pub struct ListArena {
    slots: Vec<ListSlot>,
}

impl ListArena {
    /// Allocate a fresh list slot holding the supplied elements.
    /// Returns the handle the operand stack should carry to reach
    /// this slot.
    pub fn alloc(&mut self, elements: Vec<u64>) -> Handle {
        let handle = self.slots.len() as Handle;
        self.slots.push(Arc::new(elements));
        handle
    }

    /// Read a list slot. Returns a borrowed `Arc<Vec<u64>>` so the
    /// caller pays refcount cost only on `clone()`.
    pub fn get(&self, handle: Handle) -> Result<&ListSlot, ArenaError> {
        self.slots
            .get(handle as usize)
            .ok_or_else(|| ArenaError::OutOfRange {
                handle,
                len: self.slots.len(),
            })
    }

    /// Read one element from a list slot. `IndexOutOfBounds`-style
    /// traps at the BcOp boundary lift this into
    /// [`crate::vm::BcVmError::IndexOutOfBounds`].
    pub fn get_element(&self, handle: Handle, index: i64) -> Result<u64, ArenaError> {
        let slot = self.get(handle)?;
        if index < 0 {
            return Err(ArenaError::ElementOutOfRange {
                index: index as usize,
                len: slot.len(),
            });
        }
        slot.get(index as usize)
            .copied()
            .ok_or_else(|| ArenaError::ElementOutOfRange {
                index: index as usize,
                len: slot.len(),
            })
    }

    /// Length of a list slot. Lifts into the `i64` lane the dispatch
    /// loop uses; the slot length is bounded by `i32::MAX` in practice
    /// (the bytecode compiler never emits a constant list longer than
    /// the surrounding IR allows) so the `as i64` cast is lossless.
    pub fn len_of(&self, handle: Handle) -> Result<i64, ArenaError> {
        Ok(self.get(handle)?.len() as i64)
    }

    /// Total number of allocated slots. Used by the diagnostic tests
    /// to assert the arena is reset between invocations.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

/// Per-VM arena holding [`Arc<str>`]-shaped string slots. Parked for
/// phase 4b-continuation — no dispatch arm reads this yet, but the
/// allocator + handle surface land here so the phase-4b-continuation
/// strings work can drop in without re-litigating the layout.
#[derive(Debug, Default, Clone)]
pub struct StringArena {
    slots: Vec<StringSlot>,
}

impl StringArena {
    /// Allocate a fresh string slot.
    pub fn alloc(&mut self, value: impl Into<Arc<str>>) -> Handle {
        let handle = self.slots.len() as Handle;
        self.slots.push(value.into());
        handle
    }

    /// Read a string slot.
    pub fn get(&self, handle: Handle) -> Result<&StringSlot, ArenaError> {
        self.slots
            .get(handle as usize)
            .ok_or_else(|| ArenaError::OutOfRange {
                handle,
                len: self.slots.len(),
            })
    }

    /// Code-point count of a string slot — matches tree-walker's
    /// `String::chars().count()` semantics, which is the surface area
    /// the corpus's `.length()` patterns exercise.
    pub fn len_of(&self, handle: Handle) -> Result<i64, ArenaError> {
        Ok(self.get(handle)?.chars().count() as i64)
    }

    /// Total number of allocated slots.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

/// Per-VM arena holding string-keyed dict slots. Parked for phase
/// 4b-continuation alongside [`StringArena`].
#[derive(Debug, Default, Clone)]
pub struct DictArena {
    slots: Vec<DictSlot>,
}

impl DictArena {
    /// Allocate a fresh dict slot. Entries are stored in declaration
    /// order; duplicate keys are not deduplicated (matches the
    /// tree-walker's "last-write-wins on lookup" semantics — see
    /// [`Self::lookup`]).
    pub fn alloc(&mut self, entries: Vec<(Arc<str>, u64)>) -> Handle {
        let handle = self.slots.len() as Handle;
        self.slots.push(Arc::new(entries));
        handle
    }

    /// Read a dict slot.
    pub fn get(&self, handle: Handle) -> Result<&DictSlot, ArenaError> {
        self.slots
            .get(handle as usize)
            .ok_or_else(|| ArenaError::OutOfRange {
                handle,
                len: self.slots.len(),
            })
    }

    /// Look up a key. Returns `None` on miss (caller decides whether
    /// that lifts to `Value::Null` or to an `IndexOutOfBounds` trap).
    /// Scans in reverse so duplicate keys observe last-write-wins.
    pub fn lookup(&self, handle: Handle, key: &str) -> Result<Option<u64>, ArenaError> {
        let slot = self.get(handle)?;
        for (k, v) in slot.iter().rev() {
            if k.as_ref() == key {
                return Ok(Some(*v));
            }
        }
        Ok(None)
    }

    /// Total number of allocated slots.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

/// Composite VM-side memory state. Bundled in one struct so the
/// dispatch loop borrows the arenas mutably as a unit — partial
/// borrows of the three would force every BcOp arm that touches more
/// than one arena to drop the borrows manually.
#[derive(Debug, Default, Clone)]
pub struct VmMemory {
    /// List arena — drives [`crate::op::BcOp::MakeList`] /
    /// [`crate::op::BcOp::ListGetInt`].
    pub lists: ListArena,
    /// String arena — phase 4b-continuation.
    pub strings: StringArena,
    /// Dict arena — phase 4b-continuation.
    pub dicts: DictArena,
}

impl VmMemory {
    /// Total handle count across the three arenas. Used by the
    /// instrumentation tests to assert the arenas are reset between
    /// invocations.
    pub fn total_slot_count(&self) -> usize {
        self.lists.slot_count() + self.strings.slot_count() + self.dicts.slot_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_arena_alloc_get_round_trip() {
        let mut arena = ListArena::default();
        let h0 = arena.alloc(vec![1, 2, 3]);
        let h1 = arena.alloc(vec![]);
        assert_eq!(h0, 0);
        assert_eq!(h1, 1);
        assert_eq!(arena.slot_count(), 2);
        assert_eq!(arena.len_of(h0).unwrap(), 3);
        assert_eq!(arena.len_of(h1).unwrap(), 0);
        assert_eq!(arena.get_element(h0, 0).unwrap(), 1);
        assert_eq!(arena.get_element(h0, 2).unwrap(), 3);
    }

    #[test]
    fn list_arena_get_element_out_of_range_trips() {
        let mut arena = ListArena::default();
        let h = arena.alloc(vec![10, 20]);
        let err = arena.get_element(h, 2).unwrap_err();
        assert!(matches!(err, ArenaError::ElementOutOfRange { .. }));
        let err = arena.get_element(h, -1).unwrap_err();
        assert!(matches!(err, ArenaError::ElementOutOfRange { .. }));
    }

    #[test]
    fn list_arena_out_of_range_handle_trips() {
        let arena = ListArena::default();
        let err = arena.get(0).unwrap_err();
        assert!(matches!(err, ArenaError::OutOfRange { handle: 0, len: 0 }));
    }

    #[test]
    fn string_arena_round_trip() {
        let mut arena = StringArena::default();
        let h = arena.alloc("hello");
        assert_eq!(arena.len_of(h).unwrap(), 5);
        assert_eq!(arena.get(h).unwrap().as_ref(), "hello");
        // Multi-byte code points count as one character each.
        let h2 = arena.alloc("héllo");
        assert_eq!(arena.len_of(h2).unwrap(), 5);
    }

    #[test]
    fn dict_arena_lookup_hit_and_miss() {
        let mut arena = DictArena::default();
        let h = arena.alloc(vec![
            (Arc::from("a"), 1u64),
            (Arc::from("b"), 2u64),
            // Duplicate key: last-write-wins.
            (Arc::from("a"), 99u64),
        ]);
        assert_eq!(arena.lookup(h, "a").unwrap(), Some(99));
        assert_eq!(arena.lookup(h, "b").unwrap(), Some(2));
        assert_eq!(arena.lookup(h, "c").unwrap(), None);
    }

    #[test]
    fn vm_memory_total_slot_count_aggregates() {
        let mut mem = VmMemory::default();
        mem.lists.alloc(vec![1, 2]);
        mem.lists.alloc(vec![3]);
        mem.strings.alloc("x");
        mem.dicts.alloc(vec![]);
        assert_eq!(mem.total_slot_count(), 4);
    }
}
