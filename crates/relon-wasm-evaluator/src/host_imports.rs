//! Wire the design §4 host imports onto a wasmtime `Linker`.
//!
//! Z.1 implements only the imports the W1 / W6 / W12 lowerings touch
//! (and `__relon_trap`, which is universal). The rest are registered as
//! `unreachable` stubs so the module instantiates cleanly; reaching one
//! of them at runtime trips wasmtime's trap surface — which is what we
//! want, because emitted Z.1 modules never reference them.

use anyhow::anyhow;
use wasmtime::{Caller, Linker};

use crate::host_state::HostState;

/// Project an `anyhow::Error` from the arena / memory / record helpers
/// onto a `wasmtime::Error` for the host-import trap surface. Used via
/// `.map_err(anyhow_to_wasmtime)` so each call site stays a single line.
fn anyhow_to_wasmtime(e: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::msg(e.to_string())
}

/// Register the §4 host imports on the supplied linker. Returns an
/// error only if the wasmtime `Linker::func_wrap` plumbing rejects a
/// signature, which would indicate a freeze-table mismatch.
pub(crate) fn register_host_imports(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // === §4.1 Arena / control ===
    linker.func_wrap(
        "relon",
        "__relon_arena_alloc",
        |mut caller: Caller<'_, HostState>,
         size: i32,
         align: i32|
         -> Result<i32, wasmtime::Error> {
            let ptr = caller
                .data_mut()
                .arena_alloc(size as u32, align as u32)
                .map_err(anyhow_to_wasmtime)?;
            Ok(ptr as i32)
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_arena_reset",
        |mut caller: Caller<'_, HostState>| {
            caller.data_mut().reset();
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_arena_grow",
        |_caller: Caller<'_, HostState>, _pages: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_arena_grow: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_trap",
        |_caller: Caller<'_, HostState>, code: i32| -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg(format!("__relon_trap code={code}")))
        },
    )?;

    // === §4.2 String — Z.1 stubs (only need them declared for ABI parity) ===
    for name in ["__relon_str_intern", "__relon_str_alloc"] {
        let label = name;
        linker.func_wrap(
            "relon",
            name,
            move |_caller: Caller<'_, HostState>,
                  _a: i32,
                  _b: i32|
                  -> Result<i32, wasmtime::Error> {
                Err(wasmtime::Error::msg(format!("{label}: Z.3 follow-up")))
            },
        )?;
    }
    linker.func_wrap(
        "relon",
        "__relon_str_len",
        |_caller: Caller<'_, HostState>, _a: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_str_len: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_byte_len",
        |_caller: Caller<'_, HostState>, _a: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_str_byte_len: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_eq",
        |_caller: Caller<'_, HostState>, _a: i32, _b: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_str_eq: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_concat",
        |_caller: Caller<'_, HostState>, _a: i32, _b: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_str_concat: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_concat_n",
        |_caller: Caller<'_, HostState>, _a: i32, _b: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_str_concat_n: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_contains",
        |mut caller: Caller<'_, HostState>,
         haystack_handle: i32,
         needle_handle: i32|
         -> Result<i32, wasmtime::Error> {
            str_contains(&mut caller, haystack_handle, needle_handle)
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_substring",
        |_caller: Caller<'_, HostState>,
         _a: i32,
         _b: i64,
         _c: i64|
         -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_str_substring: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_str_glob_match",
        |_caller: Caller<'_, HostState>, _a: i32, _b: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg(
                "__relon_str_glob_match: Z.3 follow-up",
            ))
        },
    )?;

    // === §4.3 List ===
    linker.func_wrap(
        "relon",
        "__relon_list_new",
        |_caller: Caller<'_, HostState>, _et: i32, _cap: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_new: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_len",
        |mut caller: Caller<'_, HostState>, handle: i32| -> Result<i64, wasmtime::Error> {
            list_header_len(&mut caller, handle)
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_push_i64",
        |_caller: Caller<'_, HostState>, _h: i32, _v: i64| -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_push_i64: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_push_f64",
        |_caller: Caller<'_, HostState>, _h: i32, _v: f64| -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_push_f64: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_get_i64",
        |mut caller: Caller<'_, HostState>,
         handle: i32,
         idx: i64|
         -> Result<i64, wasmtime::Error> { list_get_i64(&mut caller, handle, idx) },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_get_f64",
        |_caller: Caller<'_, HostState>, _h: i32, _i: i64| -> Result<f64, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_get_f64: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_set_i64",
        |_caller: Caller<'_, HostState>,
         _h: i32,
         _i: i64,
         _v: i64|
         -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_set_i64: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_set_f64",
        |_caller: Caller<'_, HostState>,
         _h: i32,
         _i: i64,
         _v: f64|
         -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_set_f64: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_range_alloc",
        |mut caller: Caller<'_, HostState>, start: i64, end: i64| -> Result<i32, wasmtime::Error> {
            range_alloc(&mut caller, start, end)
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_sum_i64",
        |mut caller: Caller<'_, HostState>, handle: i32| -> Result<i64, wasmtime::Error> {
            list_sum_i64(&mut caller, handle)
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_list_sum_f64",
        |_caller: Caller<'_, HostState>, _h: i32| -> Result<f64, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_list_sum_f64: Z.3 follow-up"))
        },
    )?;

    // === §4.4 Dict — Z.1 stubs ===
    linker.func_wrap(
        "relon",
        "__relon_dict_new",
        |_c: Caller<'_, HostState>, _cap: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_dict_new: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_dict_get_str",
        |_c: Caller<'_, HostState>, _d: i32, _k: i32| -> Result<i64, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_dict_get_str: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_dict_set_str",
        |_c: Caller<'_, HostState>,
         _d: i32,
         _k: i32,
         _t: i32,
         _v: i64|
         -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_dict_set_str: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_dict_contains_str",
        |_c: Caller<'_, HostState>, _d: i32, _k: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg(
                "__relon_dict_contains_str: Z.3 follow-up",
            ))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_dict_len",
        |_c: Caller<'_, HostState>, _d: i32| -> Result<i64, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_dict_len: Z.3 follow-up"))
        },
    )?;

    // === §4.5 Closure — Z.1 stubs ===
    linker.func_wrap(
        "relon",
        "__relon_closure_alloc",
        |_c: Caller<'_, HostState>, _f: i32, _n: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_closure_alloc: Z.3 follow-up"))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_closure_capture_set",
        |_c: Caller<'_, HostState>, _h: i32, _i: i32, _v: i64| -> Result<(), wasmtime::Error> {
            Err(wasmtime::Error::msg(
                "__relon_closure_capture_set: Z.3 follow-up",
            ))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_closure_capture_get",
        |_c: Caller<'_, HostState>, _h: i32, _i: i32| -> Result<i64, wasmtime::Error> {
            Err(wasmtime::Error::msg(
                "__relon_closure_capture_get: Z.3 follow-up",
            ))
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_closure_fn_idx",
        |_c: Caller<'_, HostState>, _h: i32| -> Result<i32, wasmtime::Error> {
            Err(wasmtime::Error::msg(
                "__relon_closure_fn_idx: Z.3 follow-up",
            ))
        },
    )?;

    // === §4.6 Native / capability — Z.1 stubs ===
    linker.func_wrap(
        "relon",
        "__relon_check_cap",
        |_c: Caller<'_, HostState>, cap: i32| -> Result<(), wasmtime::Error> {
            if cap == -1 {
                // `u32::MAX` sentinel — no cap required.
                Ok(())
            } else {
                Err(wasmtime::Error::msg(format!(
                    "__relon_check_cap({cap}): no policy installed (Z.3 follow-up)"
                )))
            }
        },
    )?;
    linker.func_wrap(
        "relon",
        "__relon_call_native",
        |_c: Caller<'_, HostState>, _f: i32, _a: i32| -> Result<i64, wasmtime::Error> {
            Err(wasmtime::Error::msg("__relon_call_native: Z.3 follow-up"))
        },
    )?;

    Ok(())
}

// =====================================================================
// =====   List host-import implementations (Z.1 surface)  =============
// =====================================================================

/// Linear-memory list header layout (§3.2.2):
///   +0  u32 len
///   +4  u32 cap
///   +8  u32 elem_tag
///   +12 u32 padding
///   +16 elements[0..]
///
/// Element width is fixed at 8 bytes per slot.
const LIST_HEADER_BYTES: u32 = 16;
const LIST_ELEM_BYTES: u32 = 8;

fn read_u32(mem: &[u8], off: u32) -> anyhow::Result<u32> {
    let off = off as usize;
    if off + 4 > mem.len() {
        return Err(anyhow!(
            "read_u32 out of range: off={off} len={}",
            mem.len()
        ));
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&mem[off..off + 4]);
    Ok(u32::from_le_bytes(bytes))
}

fn write_u32(mem: &mut [u8], off: u32, value: u32) -> anyhow::Result<()> {
    let off = off as usize;
    if off + 4 > mem.len() {
        return Err(anyhow!(
            "write_u32 out of range: off={off} len={}",
            mem.len()
        ));
    }
    mem[off..off + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn read_i64(mem: &[u8], off: u32) -> anyhow::Result<i64> {
    let off = off as usize;
    if off + 8 > mem.len() {
        return Err(anyhow!(
            "read_i64 out of range: off={off} len={}",
            mem.len()
        ));
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&mem[off..off + 8]);
    Ok(i64::from_le_bytes(bytes))
}

fn write_i64(mem: &mut [u8], off: u32, value: i64) -> anyhow::Result<()> {
    let off = off as usize;
    if off + 8 > mem.len() {
        return Err(anyhow!(
            "write_i64 out of range: off={off} len={}",
            mem.len()
        ));
    }
    mem[off..off + 8].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn range_alloc(
    caller: &mut Caller<'_, HostState>,
    start: i64,
    end: i64,
) -> Result<i32, wasmtime::Error> {
    if end < start {
        return Err(wasmtime::Error::msg(format!(
            "__relon_list_range_alloc: end {end} < start {start}"
        )));
    }
    let len = (end - start) as u64;
    if len > u32::MAX as u64 / LIST_ELEM_BYTES as u64 {
        return Err(wasmtime::Error::msg(
            "__relon_list_range_alloc: range too large for Z.1 arena",
        ));
    }
    let total_bytes = LIST_HEADER_BYTES + (len as u32) * LIST_ELEM_BYTES;
    let handle = caller
        .data_mut()
        .arena_alloc(total_bytes, 8)
        .map_err(anyhow_to_wasmtime)?;

    // Borrow memory now that the arena bump is done.
    let mem = caller.data().memory().map_err(anyhow_to_wasmtime)?;
    let mem_view = mem.data_mut(caller);
    write_u32(mem_view, handle, len as u32).map_err(anyhow_to_wasmtime)?;
    write_u32(mem_view, handle + 4, len as u32).map_err(anyhow_to_wasmtime)?;
    write_u32(mem_view, handle + 8, /* tag = 1 (i64) */ 1).map_err(anyhow_to_wasmtime)?;
    write_u32(mem_view, handle + 12, 0).map_err(anyhow_to_wasmtime)?;
    for i in 0..(len as u32) {
        let elem_off = handle + LIST_HEADER_BYTES + i * LIST_ELEM_BYTES;
        let value = start + i as i64;
        write_i64(mem_view, elem_off, value).map_err(anyhow_to_wasmtime)?;
    }
    Ok(handle as i32)
}

fn list_header_len(
    caller: &mut Caller<'_, HostState>,
    handle: i32,
) -> Result<i64, wasmtime::Error> {
    let mem = caller.data().memory().map_err(anyhow_to_wasmtime)?;
    let view = mem.data(caller);
    let len = read_u32(view, handle as u32).map_err(anyhow_to_wasmtime)?;
    Ok(len as i64)
}

fn list_get_i64(
    caller: &mut Caller<'_, HostState>,
    handle: i32,
    idx: i64,
) -> Result<i64, wasmtime::Error> {
    let mem = caller.data().memory().map_err(anyhow_to_wasmtime)?;
    let view = mem.data(caller);
    let len = read_u32(view, handle as u32).map_err(anyhow_to_wasmtime)?;
    if idx < 0 || (idx as u64) >= len as u64 {
        return Err(wasmtime::Error::msg(format!(
            "__relon_list_get_i64: idx {idx} out of range [0, {len})"
        )));
    }
    let off = handle as u32 + LIST_HEADER_BYTES + (idx as u32) * LIST_ELEM_BYTES;
    let value = read_i64(view, off).map_err(anyhow_to_wasmtime)?;
    Ok(value)
}

fn list_sum_i64(caller: &mut Caller<'_, HostState>, handle: i32) -> Result<i64, wasmtime::Error> {
    let mem = caller.data().memory().map_err(anyhow_to_wasmtime)?;
    let view = mem.data(caller);
    let len = read_u32(view, handle as u32).map_err(anyhow_to_wasmtime)?;
    let mut acc: i64 = 0;
    for i in 0..len {
        let off = handle as u32 + LIST_HEADER_BYTES + i * LIST_ELEM_BYTES;
        let value = read_i64(view, off).map_err(anyhow_to_wasmtime)?;
        acc = acc.checked_add(value).ok_or_else(|| {
            wasmtime::Error::msg(format!("__relon_list_sum_i64: overflow at idx {i}"))
        })?;
    }
    Ok(acc)
}

// =====================================================================
// =====   String host-import implementations  =========================
// =====================================================================

/// `__relon_str_contains(haystack_handle, needle_handle) -> i32` —
/// 0 / 1 result mirroring the LLVM-side `relon_llvm_str_contains_arena`
/// surface. Both handles point at `[u32 le len][payload]` records in
/// linear memory; the W4 lowering installs the records as wasm data
/// segments so each call only re-reads them, no allocation.
///
/// Z.3c-c: this shim is the only per-iter host-import call W4 makes.
/// We deliberately leave the inline-cache path the LLVM AOT side
/// carries off the table — the wasmtime call overhead dominates the
/// byte-scan on the 3-byte short haystack, and adding an IC here
/// would just push the dispatch cost back into the closure plumbing
/// without buying anything. The Z.4 follow-up can revisit once we
/// measure where this row actually lands vs LuaJIT.
fn str_contains(
    caller: &mut Caller<'_, HostState>,
    haystack_handle: i32,
    needle_handle: i32,
) -> Result<i32, wasmtime::Error> {
    let mem = caller.data().memory().map_err(anyhow_to_wasmtime)?;
    let view = mem.data(caller);
    let haystack = read_str_record(view, haystack_handle as u32)?;
    let needle = read_str_record(view, needle_handle as u32)?;
    Ok(compute_contains(haystack, needle))
}

/// Read a `[u32 le len][payload bytes]` record at `handle`. Mirrors
/// the LLVM-side `read_record` contract: returns the payload slice
/// (length-checked against linear memory) so the caller can byte-
/// scan without an extra bounds check.
fn read_str_record(view: &[u8], handle: u32) -> Result<&[u8], wasmtime::Error> {
    let len = read_u32(view, handle).map_err(anyhow_to_wasmtime)?;
    let payload_start = handle as usize + 4;
    let payload_end = payload_start
        .checked_add(len as usize)
        .ok_or_else(|| wasmtime::Error::msg("str record: ptr+len overflow"))?;
    if payload_end > view.len() {
        return Err(wasmtime::Error::msg(format!(
            "str record at {handle} extends past linear memory \
             (payload_end={payload_end}, mem={})",
            view.len()
        )));
    }
    Ok(&view[payload_start..payload_end])
}

/// Byte-scan contains decision. Mirrors the LLVM-side
/// `compute_contains` semantics: empty needle is always a hit,
/// needle-longer-than-haystack is always a miss, single-byte needle
/// goes through `slice::contains` (memchr / SIMD-backed), multi-byte
/// goes through the Two-Way matcher in `str::contains` after a UTF-8
/// validation pass.
#[inline]
fn compute_contains(haystack: &[u8], needle: &[u8]) -> i32 {
    if needle.is_empty() {
        return 1;
    }
    if needle.len() > haystack.len() {
        return 0;
    }
    if needle.len() == 1 {
        let b = needle[0];
        return i32::from(haystack.contains(&b));
    }
    let h_str = match std::str::from_utf8(haystack) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let n_str = match std::str::from_utf8(needle) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(h_str.contains(n_str))
}
