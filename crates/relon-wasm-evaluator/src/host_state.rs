//! Per-instance host state. Carries the arena cursors, IC slots, and
//! a pointer to the wasm linear memory so host imports can read /
//! write strings, lists, dicts directly.
//!
//! See `docs/internal/phase-z-design.md` §3 for the linear-memory
//! layout and §6.3 for the active-tier signal.

use wasmtime::Memory;

use crate::Tier;

/// Per-instance state threaded through every host import call.
pub struct HostState {
    /// Linear-memory handle bound at instantiate time. `None` between
    /// `WasmEvaluator::new` and the post-instantiate `bind_memory`
    /// hook; reaching a host-import call with `None` indicates a wiring
    /// bug.
    memory: Option<Memory>,
    /// Bump cursor for the per-call arena, in bytes.
    arena_cursor: u32,
    /// Floor for `arena_cursor` after `reset()`. Z.3c-c W4 lowering
    /// installs const data segments (haystack + needle records) at
    /// fixed offsets in linear memory; bumping the floor past those
    /// records prevents the per-call arena from clobbering them.
    /// `0` for variants without const segments.
    arena_floor: u32,
    /// Hard cap on the arena bump. Currently 1 MiB matching the
    /// initial-memory size; growth is `Z.3` work.
    arena_cap: u32,
    /// Module-lifetime string pool. Each entry's interned bytes live
    /// in the same linear memory the per-call arena uses, but live
    /// at the top end of the address space so the per-call reset
    /// doesn't stomp them. Z.1 doesn't yet exercise the pool — it's
    /// in place so the host imports can wire to it without churn.
    str_pool_top: u32,
    /// Current tier label surfaced via `WasmEvaluator::active_tier`.
    tier: Tier,
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

impl HostState {
    /// Fresh host state. The memory binding is wired by
    /// `WasmEvaluator::new` immediately after `Linker::instantiate`.
    pub fn new() -> Self {
        Self {
            memory: None,
            arena_cursor: 0,
            arena_floor: 0,
            arena_cap: 1024 * 1024,        // 1 MiB
            str_pool_top: 1024 * 1024 - 1, // grows downward from the top
            tier: Tier::Cold,
        }
    }

    /// Wire the wasm memory handle into the host state. Called once at
    /// instantiate time.
    pub fn bind_memory(&mut self, memory: Memory) {
        self.memory = Some(memory);
    }

    /// Reserve the linear-memory range `[0..end)` for module-installed
    /// const data segments. Bumps the arena floor so subsequent
    /// `reset()` / `arena_alloc` calls land past the const region.
    /// Safe to call multiple times — the floor only grows.
    pub fn bind_const_segment_end(&mut self, end: u32) {
        if end > self.arena_floor {
            self.arena_floor = end;
        }
        if self.arena_cursor < self.arena_floor {
            self.arena_cursor = self.arena_floor;
        }
    }

    /// Reset the per-call arena. The string pool and const-segment
    /// region (`[0..arena_floor)`) are preserved.
    pub fn reset(&mut self) {
        self.arena_cursor = self.arena_floor;
    }

    /// Snapshot of the current tier for the public `active_tier` API.
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Mark the tier as `Compiled` after a successful `run_main`.
    pub fn mark_compiled(&mut self) {
        self.tier = Tier::Compiled;
    }

    /// Mark the tier as `Deoptimised` after a wasmtime trap.
    pub fn mark_deopt(&mut self) {
        self.tier = Tier::Deoptimised;
    }

    /// Allocate `size` bytes at `align` from the per-call arena bump.
    /// Returns the linear-memory offset; traps via `Err` on cap
    /// exhaustion.
    pub fn arena_alloc(&mut self, size: u32, align: u32) -> anyhow::Result<u32> {
        let align = align.max(1);
        let aligned_cursor = (self.arena_cursor + align - 1) & !(align - 1);
        let end = aligned_cursor
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("arena_alloc: size overflow"))?;
        if end > self.arena_cap || end > self.str_pool_top {
            return Err(anyhow::anyhow!(
                "arena_alloc: out of arena (cursor {aligned_cursor} + size {size} > cap {})",
                self.arena_cap.min(self.str_pool_top)
            ));
        }
        self.arena_cursor = end;
        Ok(aligned_cursor)
    }
}

impl HostState {
    /// Hand back the bound memory for host-import write helpers.
    /// Returns `Err` when memory wasn't wired (a wiring bug).
    pub(crate) fn memory(&self) -> anyhow::Result<Memory> {
        self.memory
            .ok_or_else(|| anyhow::anyhow!("HostState: memory not yet bound"))
    }
}
