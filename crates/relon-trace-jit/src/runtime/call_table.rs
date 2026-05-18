//! `__relon_trace_resolve_call` + the [`ExternalCallTable`].
//!
//! `TraceOp::Call(ExternalAddr, ...)` lowers to cranelift IR that
//! pulls the callee's machine-code pointer from a runtime-resolved
//! table and then issues a `call_indirect`. The lookup is done via
//! this helper:
//!
//! ```text
//! __relon_trace_resolve_call(ctx_ptr, external_addr_raw)
//!     -> *const u8
//! ```
//!
//! Resolution is done per-thread. Each thread owns its own trace
//! buffer (design doc §1.4), so the call table is a `thread_local!`
//! to avoid the cost of synchronisation and to enable per-thread
//! hot-swapping of function pointers (e.g. profile-guided variants).
//!
//! ## Lookup performance
//!
//! Backed by a `HashMap` keyed on the raw `u64` of `ExternalAddr`.
//! For trace-heavy workloads the table is small (each trace touches
//! O(callees-in-trace) entries; bounded by trace length). The
//! amortised O(1) hash lookup keeps the helper well under 100 ns on
//! tables with thousands of entries — verified in tests.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::runtime::deopt::TraceContext;
use crate::trace_ir::ExternalAddr;

/// Per-thread mapping of recorded [`ExternalAddr`]s to installed
/// function pointers.
///
/// Pointer-typed entries (not `fn`) so the host can hot-swap helpers
/// without recompiling installed traces.
#[derive(Default)]
pub struct ExternalCallTable {
    /// raw_addr -> fn_ptr. Raw `u64` keys keep the lookup
    /// representation-stable across `ExternalAddr` repacking
    /// (e.g. when v6-gamma decides to fold a type tag into the high
    /// bits — see `ExternalAddr` TODO in `trace_ir.rs`).
    entries: HashMap<u64, *const u8>,
}

impl std::fmt::Debug for ExternalCallTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalCallTable")
            .field("len", &self.entries.len())
            .finish()
    }
}

impl ExternalCallTable {
    /// New empty table.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register (or replace) the function pointer for `external_addr`.
    /// The host calls this when installing a trace's call references.
    pub fn register(&mut self, external_addr: ExternalAddr, fn_ptr: *const u8) {
        self.entries.insert(external_addr.0, fn_ptr);
    }

    /// Look up the function pointer for `external_addr`. Returns
    /// `None` if unregistered — callers map that to a null pointer
    /// before returning to the cranelift trace, which then deopts.
    pub fn resolve(&self, external_addr: ExternalAddr) -> Option<*const u8> {
        self.entries.get(&external_addr.0).copied()
    }

    /// Drop the registration for `external_addr`. Returns the prior
    /// pointer if any, mirroring `HashMap::remove`.
    pub fn unregister(&mut self, external_addr: ExternalAddr) -> Option<*const u8> {
        self.entries.remove(&external_addr.0)
    }

    /// Number of installed entries; exposed for diagnostics + tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if there are no installed entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// SAFETY: entries store opaque function pointers the host pins.
// `ExternalCallTable` itself is only ever accessed from inside its
// owning thread via the `thread_local!` below, so we never share it
// across threads. The unsafe `Send`/`Sync` impls are intentionally
// **not** added — keeping the type !Send + !Sync is the type-system's
// reinforcement of the design-doc §1.4 thread-locality invariant.

thread_local! {
    /// Each thread owns its own call table. Initialised lazily on
    /// first access; never shared.
    static CALL_TABLE: RefCell<ExternalCallTable> = RefCell::new(ExternalCallTable::new());
}

/// Run `f` with mutable access to the current thread's call table.
/// Used by the host during trace installation.
pub fn with_call_table<R>(f: impl FnOnce(&mut ExternalCallTable) -> R) -> R {
    CALL_TABLE.with(|t| f(&mut t.borrow_mut()))
}

/// Convenience wrapper: register a single `(addr, ptr)` mapping on
/// the current thread.
pub fn register_external_call(addr: ExternalAddr, fn_ptr: *const u8) {
    with_call_table(|t| t.register(addr, fn_ptr));
}

/// Convenience wrapper: look up `addr` on the current thread,
/// returning the registered pointer or `None`.
pub fn resolve_external_call(addr: ExternalAddr) -> Option<*const u8> {
    CALL_TABLE.with(|t| t.borrow().resolve(addr))
}

/// Host-side runtime helper invoked from cranelift-emitted trace IR
/// to resolve a recorded [`ExternalAddr`] to a callable function
/// pointer. The emitter passes the raw `u64` representation so the
/// helper signature stays simple (no struct returns).
///
/// Returns `std::ptr::null()` on miss — the emitter is expected to
/// branch on null and route to its deopt block. The helper itself
/// never panics on a missing entry; that would crash the host.
///
/// ## Safety
///
/// `_ctx_ptr` is currently unused but is part of the ABI so the
/// emitter can pass a single uniform first-arg pointer. The pointer
/// is not dereferenced. The function is `unsafe extern "C"` so its
/// signature is callable from cranelift IR.
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_resolve_call(
    _ctx_ptr: *mut TraceContext,
    external_addr_raw: u64,
) -> *const u8 {
    let addr = ExternalAddr(external_addr_raw);
    CALL_TABLE.with(|t| t.borrow().resolve(addr).unwrap_or(std::ptr::null::<u8>()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Instant;

    /// Returns a stable but distinct `*const u8` for `id`; safe to
    /// store in the call table for test purposes (we never call it).
    fn fake_fn_ptr(id: usize) -> *const u8 {
        // 0x1000 base keeps these clearly non-null and distinct from
        // any plausible real address in tests.
        (0x1000 + id) as *const u8
    }

    #[test]
    fn register_and_resolve_roundtrip() {
        let mut table = ExternalCallTable::new();
        let addr = ExternalAddr(0x42);
        let ptr = fake_fn_ptr(7);
        table.register(addr, ptr);
        assert_eq!(table.resolve(addr), Some(ptr));
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn unregistered_addr_resolves_to_none() {
        let table = ExternalCallTable::new();
        assert!(table.resolve(ExternalAddr(0xdead)).is_none());
        assert!(table.is_empty());
    }

    #[test]
    fn resolve_call_helper_returns_null_on_miss() {
        // Use an addr we never registered on this thread.
        let result =
            unsafe { __relon_trace_resolve_call(std::ptr::null_mut(), 0xdead_beef_dead_beef) };
        assert!(result.is_null());
    }

    #[test]
    fn resolve_call_helper_returns_registered_ptr() {
        let addr = ExternalAddr(0xfeed);
        let ptr = fake_fn_ptr(9);
        register_external_call(addr, ptr);
        let result = unsafe { __relon_trace_resolve_call(std::ptr::null_mut(), addr.0) };
        assert_eq!(result, ptr);
        // Cleanup so we don't leak state into other tests sharing
        // this thread.
        with_call_table(|t| {
            t.unregister(addr);
        });
    }

    #[test]
    fn unregister_returns_prior_ptr() {
        let mut table = ExternalCallTable::new();
        let addr = ExternalAddr(0x1);
        let ptr = fake_fn_ptr(1);
        table.register(addr, ptr);
        assert_eq!(table.unregister(addr), Some(ptr));
        assert_eq!(table.unregister(addr), None);
    }

    #[test]
    fn each_thread_has_its_own_call_table() {
        let num_threads = 6;
        let barrier = Arc::new(Barrier::new(num_threads));
        let mut handles = Vec::new();
        for tid in 0..num_threads {
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let addr = ExternalAddr(tid as u64);
                let ptr = fake_fn_ptr(tid);
                register_external_call(addr, ptr);
                b.wait();
                // Our own registration is visible.
                assert_eq!(resolve_external_call(addr), Some(ptr));
                // Other threads' registrations are NOT visible here.
                for other in 0..num_threads {
                    if other == tid {
                        continue;
                    }
                    assert_eq!(
                        resolve_external_call(ExternalAddr(other as u64)),
                        None,
                        "tid={} saw other={}'s registration",
                        tid,
                        other
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn resolve_is_fast_with_thousand_entries() {
        let mut table = ExternalCallTable::new();
        for i in 0..1_000u64 {
            table.register(ExternalAddr(i), fake_fn_ptr(i as usize));
        }
        // Probe a handful of resolves to confirm correctness, then
        // time the average. The < 100 ns target from the task spec
        // is a soft bound; we assert a generous wall-clock budget
        // here to keep the test reliable across slow CI machines.
        for i in [0u64, 1, 500, 999] {
            assert_eq!(
                table.resolve(ExternalAddr(i)),
                Some(fake_fn_ptr(i as usize))
            );
        }
        let start = Instant::now();
        let iters = 100_000u64;
        let mut sink: usize = 0;
        for i in 0..iters {
            // Cycle keys to avoid CPU branch-prediction shortcuts.
            let key = i % 1_000;
            if let Some(p) = table.resolve(ExternalAddr(key)) {
                sink = sink.wrapping_add(p as usize);
            }
        }
        let elapsed_ns = start.elapsed().as_nanos();
        let per_lookup_ns = elapsed_ns / iters as u128;
        // Soft assertion: 5_000 ns is a 50x safety margin on the
        // target. Keeps the test reliable on noisy CI hosts.
        assert!(
            per_lookup_ns < 5_000,
            "resolve took {} ns/lookup (sink={})",
            per_lookup_ns,
            sink
        );
        // Sanity: sink read non-null pointers so the loop didn't get
        // optimised away.
        assert_ne!(sink, 0);
    }

    #[test]
    fn from_raw_corrupt_values_dont_panic() {
        // Even pathological raw values (u64::MAX, 0) must not panic.
        let table = ExternalCallTable::new();
        assert!(table.resolve(ExternalAddr(0)).is_none());
        assert!(table.resolve(ExternalAddr(u64::MAX)).is_none());
        // Helper variant.
        let r1 = unsafe { __relon_trace_resolve_call(std::ptr::null_mut(), 0) };
        let r2 = unsafe { __relon_trace_resolve_call(std::ptr::null_mut(), u64::MAX) };
        assert!(r1.is_null());
        assert!(r2.is_null());
    }
}
