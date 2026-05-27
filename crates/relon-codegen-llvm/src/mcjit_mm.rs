//! Phase L codegen-quality: custom [`McjitMemoryManager`] for MCJIT.
//!
//! ## Why this exists
//!
//! Inkwell's `create_jit_execution_engine` calls
//! [`LLVMCreateJITCompilerForModule`] which hard-codes
//! `CodeModel::JITDefault` — on x86_64 Linux that resolves to
//! `CodeModel::Large`. The Large model forces every cross-function
//! call (including same-module self-recursion) through a 64-bit
//! absolute pointer:
//!
//! ```text
//!   movabsq $relon_lambda_0___closure_0, %r14
//!   callq   *%r14
//! ```
//!
//! For tight recursive bodies (W7 fib lambda, fib(22) ≈ 35k calls)
//! the indirect call adds ~0.2 ns / call vs the direct
//! `callq <pcrel32>` form the equivalent rustc LTO build emits.
//! Multiplied across the recursion tree this widens the LLVM-AOT vs
//! rustc-native gap by ~10 µs on the cmp_lua W7 row.
//!
//! ## Approach
//!
//! Call the `_with_memory_manager` constructor with a custom mem
//! manager and pass `CodeModel::Small`. The runtime dynamic linker
//! still resolves each `R_X86_64_PLT32` relocation; the constraint is
//! "caller and callee within ±2 GB" which holds trivially for
//! intra-module calls (same `.text` block).
//!
//! ## Code allocation strategy
//!
//! Plain `mmap` with `PROT_READ | PROT_WRITE` (later promoted to
//! `PROT_READ | PROT_EXEC` via `finalize_memory`). We do **not** use
//! `MAP_32BIT` — Small CodeModel only needs cross-section
//! displacements to fit in 32 bits, and we ensure that by allocating
//! all code sections from one contiguous arena. Externs that live in
//! the host process binary (e.g. the `relon_llvm_str_contains_arena`
//! shim) are an explicit non-goal for the Small-mode path; the host
//! falls back to `JITDefault` whenever the module references an
//! extern symbol. See `should_use_small_code_model` in `evaluator.rs`.
//!
//! ## Scope
//!
//! - Code arena: single anonymous `mmap`, doubled on demand. Tracks
//!   regions so `finalize_memory` can `mprotect` them to RX, and
//!   `destroy` can `munmap` cleanly.
//! - Data sections: separate `mmap` per allocation (kept RW). Most
//!   modules emit a single tiny `.rodata` for the const pool; we
//!   don't bother sub-allocating.
//! - No EH frame / unwinding — the lambda bodies don't `panic`, the
//!   host catches via the engine's trampoline.

use std::cell::RefCell;
use std::rc::Rc;

use inkwell::memory_manager::McjitMemoryManager;
use libc::{c_int, c_uint, c_void, size_t};

/// Default code-arena size hint. Picked so the W1 / W2 / W7 / W11
/// modules fit in one mmap region with room for future growth. We
/// only ever allocate one region per MCJIT engine in practice.
const DEFAULT_CODE_ARENA_BYTES: size_t = 256 * 1024;

#[derive(Debug)]
struct MmapRegion {
    base: *mut u8,
    size: size_t,
}

#[derive(Debug)]
pub struct ContiguousCodeMemoryManager {
    /// Code regions allocated for `.text` sections. Each region is
    /// `mmap`'d RW, flipped to RX in `finalize_memory`, and `munmap`'d
    /// in `destroy`.
    code_regions: Rc<RefCell<Vec<MmapRegion>>>,
    /// Per-region cursor (bytes consumed). Used when LLVM asks for
    /// multiple code sections — we bump-allocate within the current
    /// region so all `.text` sections stay within ±2 GB of each other.
    code_cursor: Rc<RefCell<size_t>>,
    /// Data sections — each gets its own `mmap`. Stays RW.
    data_regions: Rc<RefCell<Vec<MmapRegion>>>,
    /// `false` until `finalize_memory` has been called at least once.
    /// Subsequent allocations after finalize are not legal under MCJIT's
    /// lifecycle but we tolerate them by leaving them RW.
    finalized: Rc<RefCell<bool>>,
}

impl ContiguousCodeMemoryManager {
    pub fn new() -> Self {
        Self {
            code_regions: Rc::new(RefCell::new(Vec::new())),
            code_cursor: Rc::new(RefCell::new(0)),
            data_regions: Rc::new(RefCell::new(Vec::new())),
            finalized: Rc::new(RefCell::new(false)),
        }
    }

    fn ensure_code_region(&self, need: size_t, alignment: c_uint) -> *mut u8 {
        let mut regions = self.code_regions.borrow_mut();
        let mut cursor = self.code_cursor.borrow_mut();

        // Try the most recent region first.
        if let Some(region) = regions.last() {
            // Align cursor up to `alignment`.
            let align = std::cmp::max(alignment as size_t, 16);
            let aligned = (*cursor + align - 1) & !(align - 1);
            if aligned + need <= region.size {
                let ptr = unsafe { region.base.add(aligned) };
                *cursor = aligned + need;
                return ptr;
            }
        }

        // Need a new region. Round `need` up to the default arena
        // size; if the request itself is larger, round up to page size.
        let page = 4096;
        let want = std::cmp::max(need, DEFAULT_CODE_ARENA_BYTES);
        let want = (want + page - 1) & !(page - 1);

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                want,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1 as c_int,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return std::ptr::null_mut();
        }
        let base = base as *mut u8;
        // Fresh region: cursor starts at 0; the first allocation lives
        // at offset 0 because anonymous `mmap` already returns page-
        // aligned addresses (4096-aligned >> any code section alignment
        // LLVM asks for in practice — 16-byte for x86_64 `.text`).
        let ptr = base;
        *cursor = need;
        regions.push(MmapRegion { base, size: want });
        ptr
    }
}

impl Default for ContiguousCodeMemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McjitMemoryManager for ContiguousCodeMemoryManager {
    fn allocate_code_section(
        &mut self,
        size: size_t,
        alignment: c_uint,
        _section_id: c_uint,
        _section_name: &str,
    ) -> *mut u8 {
        let alignment = if alignment == 0 { 16 } else { alignment };
        self.ensure_code_region(size, alignment)
    }

    fn allocate_data_section(
        &mut self,
        size: size_t,
        alignment: c_uint,
        _section_id: c_uint,
        _section_name: &str,
        _is_read_only: bool,
    ) -> *mut u8 {
        let alignment = if alignment == 0 { 8 } else { alignment };
        let page = 4096;
        let want = std::cmp::max(size, alignment as size_t);
        let want = (want + page - 1) & !(page - 1);
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                want,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1 as c_int,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return std::ptr::null_mut();
        }
        let base = base as *mut u8;
        self.data_regions
            .borrow_mut()
            .push(MmapRegion { base, size: want });
        base
    }

    fn finalize_memory(&mut self) -> Result<(), String> {
        // Flip code regions RW -> RX.
        let regions = self.code_regions.borrow();
        for region in regions.iter() {
            let ret = unsafe {
                libc::mprotect(
                    region.base as *mut c_void,
                    region.size,
                    libc::PROT_READ | libc::PROT_EXEC,
                )
            };
            if ret != 0 {
                let errno = std::io::Error::last_os_error();
                return Err(format!(
                    "mprotect RX failed for code region {:p} (size {}): {errno}",
                    region.base, region.size
                ));
            }
        }
        *self.finalized.borrow_mut() = true;
        Ok(())
    }

    fn destroy(&mut self) {
        // munmap every region we own. The MCJIT engine drops us
        // once the engine itself is dropped, so any further use is
        // undefined.
        for region in self.code_regions.borrow_mut().drain(..) {
            unsafe {
                libc::munmap(region.base as *mut c_void, region.size);
            }
        }
        for region in self.data_regions.borrow_mut().drain(..) {
            unsafe {
                libc::munmap(region.base as *mut c_void, region.size);
            }
        }
    }
}
