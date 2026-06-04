//! P-clock / P-random — built-in WASI-backed capability primitives.
//!
//! Two source-level intrinsics, `clock()` and `random()`, lower to the
//! IR ops `Op::ReadClock` / `Op::ReadRandom` (each `[] -> [I64]`), with
//! a preceding `Op::CheckCap` carrying the capability bit
//! (`reads_clock` / `uses_rng`). Both ops produce a non-deterministic
//! `Int` and are lowered here per target:
//!
//! ## native target
//!
//! Lowered to a `call` of a host-resident `extern "C"` helper resolved
//! at MCJIT link time via `engine.add_global_mapping` —
//! `clock()` → `relon_llvm_read_clock_ns() -> i64` (SystemTime),
//! `random()` → `relon_llvm_read_random_i64() -> i64` (/dev/urandom).
//! Same mechanism as the dynamic `relon_llvm_call_native` dispatch
//! helper (see `crate::state`).
//!
//! ## wasm32 target — STANDARD WASI preview1 import
//!
//! Productionizes the `tests/aot_wasm_std_wasi.rs` spike. The op lowers
//! to a **standard WASI** import (NOT a relon-custom `env::*` import),
//! satisfied by any off-the-shelf WASI host (`wasmtime-wasi`):
//!
//!   * `clock()`  → `(import "wasi_snapshot_preview1" "clock_time_get"
//!                    (func (param i32 i64 i32) (result i32)))`
//!     ABI: `clock_time_get(clock_id, precision, *time) -> errno`. We
//!     pass `CLOCK_REALTIME=0`, `precision=0`, and a 8-byte linear-
//!     memory scratch slot (`alloca`); on `errno==0` we load the `u64`
//!     nanosecond timestamp the host wrote and push it.
//!   * `random()` → `(import "wasi_snapshot_preview1" "random_get"
//!                    (func (param i32 i32) (result i32)))`
//!     ABI: `random_get(*buf, len) -> errno`. We pass a 8-byte scratch
//!     slot + `len=8`; on `errno==0` we load the `u64` and push it.
//!
//! The import is retargeted off the default `env` module onto standard
//! WASI by the LLVM `wasm-import-module` / `wasm-import-name` function
//! attributes — the whole codegen crux §8 calls out. The scratch slot
//! is an `alloca` in the entry block: on wasm32 LLVM lowers `alloca` to
//! a linear-memory stack slot, and `ptrtoint`/`inttoptr` give the i32
//! linear-memory address the WASI ABI expects.
//!
//! On a non-zero errno the lowering pushes `0` (degraded but
//! well-defined) — a standard WASI host returns `0` for the supported
//! clocks, so this path is not exercised in practice.

use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, PointerValue};
use inkwell::AddressSpace;
use inkwell::IntPredicate;

use relon_ir::ir::IrType;

use crate::codegen::Emit;
use crate::error::LlvmError;

/// CLOCK_REALTIME clock id for `clock_time_get` (WASI preview1).
const WASI_CLOCK_REALTIME: u64 = 0;
/// WASI preview1 import module name (standard, ecosystem-native).
const WASI_MODULE: &str = "wasi_snapshot_preview1";
/// The conventional first preopened directory fd for `path_open`: stdio
/// occupies 0/1/2, the first `preopened_dir` lands at 3.
const WASI_PREOPEN_DIRFD: u64 = 3;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Lower `Op::ReadClock`: built-in `clock()` primitive.
    pub(crate) fn emit_read_clock(&mut self) -> Result<(), LlvmError> {
        match self.target {
            crate::CodegenTarget::Wasm32 => self.emit_read_clock_wasi(),
            crate::CodegenTarget::Native => self
                .emit_read_native_helper(crate::state::RELON_LLVM_READ_CLOCK_SYMBOL, "read_clock"),
        }
    }

    /// Lower `Op::ReadRandom`: built-in `random()` primitive.
    pub(crate) fn emit_read_random(&mut self) -> Result<(), LlvmError> {
        match self.target {
            crate::CodegenTarget::Wasm32 => self.emit_read_random_wasi(),
            crate::CodegenTarget::Native => self.emit_read_native_helper(
                crate::state::RELON_LLVM_READ_RANDOM_SYMBOL,
                "read_random",
            ),
        }
    }

    /// Lower `Op::ReadFile`: built-in `read_file(path) -> String`
    /// primitive (P-fs Stage 1). Pops the path String (an arena-relative
    /// i32 offset) and pushes the contents String (also an arena-relative
    /// i32 offset).
    pub(crate) fn emit_read_file(&mut self) -> Result<(), LlvmError> {
        match self.target {
            crate::CodegenTarget::Wasm32 => self.emit_read_file_wasi(),
            crate::CodegenTarget::Native => self.emit_read_file_native(),
        }
    }

    /// Lower `Op::ReadDir`: built-in `read_dir(path) -> List<String>`
    /// primitive (P-fs Stage 2). Pops the path String (an arena-relative
    /// i32 offset) and pushes the List<String> of sorted entry names
    /// (also an arena-relative i32 offset, pointing at the header record).
    ///
    /// Native-only: wasm32 (`fd_readdir` dirent-stream protocol) is NOT
    /// yet implemented and raises a loud codegen error rather than emit a
    /// silent / incorrect listing.
    pub(crate) fn emit_read_dir(&mut self) -> Result<(), LlvmError> {
        match self.target {
            crate::CodegenTarget::Wasm32 => Err(LlvmError::Codegen(
                "Op::ReadDir (read_dir) is not yet implemented on wasm32: the WASI preview1 \
                 `fd_readdir` dirent-stream protocol (paged cookie loop + in-linear-memory \
                 sort of variable-length names) is deferred to a later P-fs stage. \
                 read_dir is supported on the native backends (tree-walk / cranelift / \
                 llvm-native) only."
                    .into(),
            )),
            crate::CodegenTarget::Native => self.emit_read_dir_native(),
        }
    }

    /// Native: `call @relon_llvm_read_dir(state, path_off) -> i32`. The
    /// helper reads the path, lists + sorts the entries, materializes a
    /// List<String> record at the scratch cursor, and returns its offset
    /// (or a negative sentinel + `state.trap_code` on failure). Identical
    /// trap-on-trap_code shape to `emit_read_file_native`, only the
    /// pushed value type differs (`ListString`).
    fn emit_read_dir_native(&mut self) -> Result<(), LlvmError> {
        let i8_t = self.ctx.i8_type();
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());

        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::ReadDir requires the buffer-protocol entry (no state_ptr available)".into(),
            )
        })?;

        let path_off = self.pop("ReadDir")?.val;

        let symbol = crate::state::RELON_LLVM_READ_DIR_SYMBOL;
        let helper = match self.module.get_function(symbol) {
            Some(f) => f,
            None => {
                let fn_ty = i32_t.fn_type(&[ptr_t.into(), i32_t.into()], false);
                self.module
                    .add_function(symbol, fn_ty, Some(inkwell::module::Linkage::External))
            }
        };
        let call_site = self
            .builder
            .build_call(
                helper,
                &[state_ptr.into(), path_off.into()],
                &self.next_name("read_dir"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_dir build_call: {e}")))?;
        let result_off = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "read_dir helper returned {other:?}, expected i32"
                )));
            }
        };

        // Load `state.trap_code`; non-zero means the listing failed.
        let trap_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t
                        .const_int(u64::from(crate::state::ARENA_STATE_OFFSET_TRAP_CODE), false)],
                    &self.next_name("rd_trap_gep"),
                )
                .map_err(|e| LlvmError::Codegen(format!("read_dir trap_code gep: {e}")))?
        };
        let trap_code = self
            .builder
            .build_load(i64_t, trap_gep, &self.next_name("rd_trap_code"))
            .map_err(|e| LlvmError::Codegen(format!("read_dir trap_code load: {e}")))?
            .into_int_value();
        let zero = i64_t.const_zero();
        let trapped = self
            .builder
            .build_int_compare(
                IntPredicate::NE,
                trap_code,
                zero,
                &self.next_name("rd_trapped"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_dir trap cmp: {e}")))?;
        let trap_bb = self.ctx.append_basic_block(self.func, "rd_trap");
        let cont_bb = self.ctx.append_basic_block(self.func, "rd_cont");
        self.builder
            .build_conditional_branch(trapped, trap_bb, cont_bb)
            .map_err(|e| LlvmError::Codegen(format!("read_dir trap branch: {e}")))?;
        self.builder.position_at_end(trap_bb);
        self.emit_trap_sentinel_return("ReadDir")?;
        self.builder.position_at_end(cont_bb);

        // The result is the arena-relative offset of the List<String>.
        self.push(result_off, IrType::ListString);
        Ok(())
    }

    /// Native: `call @relon_llvm_read_file(state, path_off) -> i32`. The
    /// helper reads the path out of the arena, resolves it against the
    /// shared sandbox root, reads the file, bump-allocates a String
    /// record at `tail_cursor`, and returns its offset (or a negative
    /// sentinel + `state.trap_code` on failure). Mirrors the dynamic
    /// `Op::CallNative` trap-on-trap_code shape.
    fn emit_read_file_native(&mut self) -> Result<(), LlvmError> {
        let i8_t = self.ctx.i8_type();
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());

        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::ReadFile requires the buffer-protocol entry (no state_ptr available)".into(),
            )
        })?;

        // Pop the path String operand (an i32 arena offset).
        let path_off = self.pop("ReadFile")?.val;

        let symbol = crate::state::RELON_LLVM_READ_FILE_SYMBOL;
        let helper = match self.module.get_function(symbol) {
            Some(f) => f,
            None => {
                // (state: ptr, path_off: i32) -> i32
                let fn_ty = i32_t.fn_type(&[ptr_t.into(), i32_t.into()], false);
                self.module
                    .add_function(symbol, fn_ty, Some(inkwell::module::Linkage::External))
            }
        };
        let call_site = self
            .builder
            .build_call(
                helper,
                &[state_ptr.into(), path_off.into()],
                &self.next_name("read_file"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file build_call: {e}")))?;
        let result_off = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "read_file helper returned {other:?}, expected i32"
                )));
            }
        };

        // Load `state.trap_code`; a non-zero value means the read failed
        // (sandbox escape / I/O error / arena overflow). Route to the
        // trap epilogue (helper already stored the precise code).
        let trap_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t
                        .const_int(u64::from(crate::state::ARENA_STATE_OFFSET_TRAP_CODE), false)],
                    &self.next_name("rf_trap_gep"),
                )
                .map_err(|e| LlvmError::Codegen(format!("read_file trap_code gep: {e}")))?
        };
        let trap_code = self
            .builder
            .build_load(i64_t, trap_gep, &self.next_name("rf_trap_code"))
            .map_err(|e| LlvmError::Codegen(format!("read_file trap_code load: {e}")))?
            .into_int_value();
        let zero = i64_t.const_zero();
        let trapped = self
            .builder
            .build_int_compare(
                IntPredicate::NE,
                trap_code,
                zero,
                &self.next_name("rf_trapped"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file trap cmp: {e}")))?;
        let trap_bb = self.ctx.append_basic_block(self.func, "rf_trap");
        let cont_bb = self.ctx.append_basic_block(self.func, "rf_cont");
        self.builder
            .build_conditional_branch(trapped, trap_bb, cont_bb)
            .map_err(|e| LlvmError::Codegen(format!("read_file trap branch: {e}")))?;
        self.builder.position_at_end(trap_bb);
        self.emit_trap_sentinel_return("ReadFile")?;
        self.builder.position_at_end(cont_bb);

        // The result is the arena-relative offset of the contents String.
        self.push(result_off, IrType::String);
        Ok(())
    }

    /// wasm32 `read_file(path)` → standard preview1 fd protocol
    /// (`path_open` -> `fd_read` -> `fd_close`) against a preopened dir,
    /// writing the contents into a fresh arena String record.
    ///
    /// ## fd protocol (mirrors `tests/aot_wasm_wasi_fs_spike.rs`)
    ///
    /// The path operand is a String handle: an arena-relative i32 offset
    /// to a `[len:u32 LE][utf8]` record. We read its `len` and point
    /// `path_open` at the payload (`arena_base + path_off + 4`) — the
    /// path bytes already live in linear memory.
    ///
    ///   * `path_open(dirfd=3, dirflags=0, path_ptr, path_len, oflags=0,
    ///                fs_rights_base=RIGHTS_FD_READ|FD_SEEK,
    ///                fs_rights_inheriting=same, fdflags=0, *fd_out)` —
    ///     the conventional first preopened dir is fd 3 (stdio 0/1/2).
    ///   * a single iovec `{buf: i32, len: i32}` whose `buf` is the
    ///     content record's **payload** (`record+4`) and `len` is the
    ///     remaining scratch budget;
    ///   * `fd_read(fd, *iovec, 1, *nread)`;
    ///   * `fd_close(fd)`.
    ///
    /// ## iovec ↔ arena String-out reconciliation
    ///
    /// To stay bit-equal with the native arm's scratch-String-out
    /// convention, the contents land in a fresh String record bump-
    /// allocated in the **scratch region** (`scratch_base +
    /// scratch_cursor`, 4-aligned) shaped `[len:u32][utf8]`. The iovec's
    /// `buf` points at `record+4`, so `fd_read` writes the file bytes
    /// straight into the record payload. Afterwards we store the host-
    /// written `nread` into the record's `[len]` header and bump
    /// `scratch_cursor` past `4 + nread`. Pushing the record's arena-
    /// relative offset as a `String` operand makes the existing return-
    /// store path (`emit_store_field_pointer_indirect`) copy it out
    /// verbatim — exactly like the native helper's return value.
    ///
    /// The fd_out / iovec / nread WASI marshalling slots are entry-block
    /// `alloca`s (linear-memory stack slots on wasm32); `ptrtoint` gives
    /// the i32 linear-memory addresses the WASI ABI expects.
    ///
    /// On any non-zero errno (open / read failure, e.g. a sandbox-denied
    /// path the preopened host refuses) the lowering records
    /// `NativeTrap::CapabilityDenied` in `state.trap_code` and routes to
    /// the trap epilogue — the same host-observable outcome the native
    /// arms raise for an unreadable / escaping path.
    fn emit_read_file_wasi(&mut self) -> Result<(), LlvmError> {
        use crate::state::{
            ARENA_STATE_OFFSET_LEN, ARENA_STATE_OFFSET_SCRATCH_BASE,
            ARENA_STATE_OFFSET_SCRATCH_CURSOR, ARENA_STATE_OFFSET_TRAP_CODE,
        };

        let i8_t = self.ctx.i8_type();
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());

        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::ReadFile wasm32 requires the buffer-protocol entry (no state_ptr available)"
                    .into(),
            )
        })?;
        // arena_base_ptr is consumed indirectly through arena_addr_i32.
        self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("Op::ReadFile wasm32 outside buffer-protocol entry shape".into())
        })?;

        // Pop the path String operand (an arena-relative i32 offset to a
        // `[len:u32][utf8]` record).
        let path_off = self.pop_int("ReadFile")?;

        // path_len = *(arena_base + path_off); path payload at +4.
        let path_len_ptr = self.arena_addr_i32(path_off)?;
        let path_len = self
            .builder
            .build_load(i32_t, path_len_ptr, &self.next_name("rf_path_len"))
            .map_err(|e| LlvmError::Codegen(format!("read_file path_len load: {e}")))?
            .into_int_value();
        let path_payload_off = self
            .builder
            .build_int_add(
                path_off,
                i32_t.const_int(4, false),
                &self.next_name("rf_path_payload_off"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file path payload off: {e}")))?;
        let path_payload_ptr = self.arena_addr_i32(path_payload_off)?;

        // Reserve the content String record at `scratch_base +
        // scratch_cursor`, 4-aligned. The record header occupies 4 bytes;
        // `fd_read` writes the file bytes into the payload at `record+4`.
        let load_state_u32 = |this: &mut Self, off: u32, hint: &str| -> Result<_, LlvmError> {
            let gep = unsafe {
                this.builder
                    .build_in_bounds_gep(
                        i8_t,
                        state_ptr,
                        &[i32_t.const_int(u64::from(off), false)],
                        &this.next_name(&format!("{hint}_gep")),
                    )
                    .map_err(|e| LlvmError::Codegen(format!("read_file {hint} gep: {e}")))?
            };
            let v = this
                .builder
                .build_load(i32_t, gep, &this.next_name(hint))
                .map_err(|e| LlvmError::Codegen(format!("read_file {hint} load: {e}")))?
                .into_int_value();
            Ok((gep, v))
        };
        let (cursor_gep, scratch_cursor) =
            load_state_u32(self, ARENA_STATE_OFFSET_SCRATCH_CURSOR, "rf_scratch_cursor")?;
        let (_, scratch_base) =
            load_state_u32(self, ARENA_STATE_OFFSET_SCRATCH_BASE, "rf_scratch_base")?;
        let (_, arena_len) = load_state_u32(self, ARENA_STATE_OFFSET_LEN, "rf_arena_len")?;

        // record_off = scratch_base + align_up(scratch_cursor, 4).
        let aligned_cursor = self.align_up_const(scratch_cursor, 0, 4, "rf_cursor")?;
        let record_off = self
            .builder
            .build_int_add(
                scratch_base,
                aligned_cursor,
                &self.next_name("rf_record_off"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file record off: {e}")))?;
        let payload_off = self
            .builder
            .build_int_add(
                record_off,
                i32_t.const_int(4, false),
                &self.next_name("rf_payload_off"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file payload off: {e}")))?;
        let payload_ptr = self.arena_addr_i32(payload_off)?;
        // Absolute (linear-memory) i32 address of the payload for the
        // iovec `buf` field.
        let payload_addr_i32 = self
            .builder
            .build_ptr_to_int(payload_ptr, i32_t, &self.next_name("rf_payload_addr"))
            .map_err(|e| LlvmError::Codegen(format!("read_file payload ptrtoint: {e}")))?;
        // Read capacity = arena_len - payload_off (room left in the arena
        // for the file bytes). `fd_read` will read at most this many.
        let capacity = self
            .builder
            .build_int_sub(arena_len, payload_off, &self.next_name("rf_capacity"))
            .map_err(|e| LlvmError::Codegen(format!("read_file capacity sub: {e}")))?;

        // WASI marshalling scratch: fd_out (i32), iovec {buf,len} (8B),
        // nread (i32). Entry-block allocas → linear-memory stack slots.
        let fd_out_slot = self.alloc_entry(i32_t.into(), "rf_fd_out")?;
        let iovec_slot = self.alloc_entry(i64_t.into(), "rf_iovec")?; // {i32 buf, i32 len}
        let nread_slot = self.alloc_entry(i32_t.into(), "rf_nread")?;

        // --- path_open(dirfd=3, 0, path_ptr, path_len, 0, rights,
        //               rights, 0, *fd_out) -> errno ---
        let path_open_ty = i32_t.fn_type(
            &[
                i32_t.into(), // dirfd
                i32_t.into(), // dirflags
                ptr_t.into(), // path_ptr
                i32_t.into(), // path_len
                i32_t.into(), // oflags
                i64_t.into(), // fs_rights_base
                i64_t.into(), // fs_rights_inheriting
                i32_t.into(), // fdflags
                ptr_t.into(), // fd_out
            ],
            false,
        );
        let path_open = self.declare_wasi_import("path_open", path_open_ty);
        // RIGHTS_FD_READ (bit 1) | RIGHTS_FD_SEEK (bit 2): the strict host
        // rejects unknown rights bits, so pass exactly what a read needs.
        let rights = i64_t.const_int((1 << 1) | (1 << 2), false);
        let open_errno = self
            .builder
            .build_call(
                path_open,
                &[
                    i32_t.const_int(WASI_PREOPEN_DIRFD, false).into(),
                    i32_t.const_zero().into(),
                    path_payload_ptr.into(),
                    path_len.into(),
                    i32_t.const_zero().into(),
                    rights.into(),
                    rights.into(),
                    i32_t.const_zero().into(),
                    fd_out_slot.into(),
                ],
                &self.next_name("rf_open_errno"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file path_open call: {e}")))?;
        let open_errno = match open_errno.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "read_file path_open returned {other:?}, expected i32 errno"
                )));
            }
        };
        let open_failed = self
            .builder
            .build_int_compare(
                IntPredicate::NE,
                open_errno,
                i32_t.const_zero(),
                &self.next_name("rf_open_failed"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file open cmp: {e}")))?;
        let open_ok_bb = self.ctx.append_basic_block(self.func, "rf_open_ok");
        let trap_bb = self.ctx.append_basic_block(self.func, "rf_wasm_trap");
        let cont_bb = self.ctx.append_basic_block(self.func, "rf_wasm_cont");
        self.builder
            .build_conditional_branch(open_failed, trap_bb, open_ok_bb)
            .map_err(|e| LlvmError::Codegen(format!("read_file open branch: {e}")))?;

        // --- open_ok: build iovec, fd_read, fd_close ---
        self.builder.position_at_end(open_ok_bb);
        let fd = self
            .builder
            .build_load(i32_t, fd_out_slot, &self.next_name("rf_fd"))
            .map_err(|e| LlvmError::Codegen(format!("read_file fd load: {e}")))?
            .into_int_value();
        // iovec.buf = payload_addr; iovec.len = capacity.
        self.builder
            .build_store(iovec_slot, payload_addr_i32)
            .map_err(|e| LlvmError::Codegen(format!("read_file iovec buf store: {e}")))?;
        let iovec_len_ptr = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    iovec_slot,
                    &[i32_t.const_int(4, false)],
                    &self.next_name("rf_iovec_len_gep"),
                )
                .map_err(|e| LlvmError::Codegen(format!("read_file iovec len gep: {e}")))?
        };
        self.builder
            .build_store(iovec_len_ptr, capacity)
            .map_err(|e| LlvmError::Codegen(format!("read_file iovec len store: {e}")))?;

        let fd_read_ty = i32_t.fn_type(
            &[i32_t.into(), ptr_t.into(), i32_t.into(), ptr_t.into()],
            false,
        );
        let fd_read = self.declare_wasi_import("fd_read", fd_read_ty);
        let read_errno = self
            .builder
            .build_call(
                fd_read,
                &[
                    fd.into(),
                    iovec_slot.into(),
                    i32_t.const_int(1, false).into(),
                    nread_slot.into(),
                ],
                &self.next_name("rf_read_errno"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file fd_read call: {e}")))?;
        let read_errno = match read_errno.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "read_file fd_read returned {other:?}, expected i32 errno"
                )));
            }
        };
        // Close regardless of read errno (best-effort).
        let fd_close_ty = i32_t.fn_type(&[i32_t.into()], false);
        let fd_close = self.declare_wasi_import("fd_close", fd_close_ty);
        self.builder
            .build_call(fd_close, &[fd.into()], &self.next_name("rf_close"))
            .map_err(|e| LlvmError::Codegen(format!("read_file fd_close call: {e}")))?;
        let read_failed = self
            .builder
            .build_int_compare(
                IntPredicate::NE,
                read_errno,
                i32_t.const_zero(),
                &self.next_name("rf_read_failed"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file read cmp: {e}")))?;
        let read_ok_bb = self.ctx.append_basic_block(self.func, "rf_read_ok");
        self.builder
            .build_conditional_branch(read_failed, trap_bb, read_ok_bb)
            .map_err(|e| LlvmError::Codegen(format!("read_file read branch: {e}")))?;

        // --- read_ok: stamp the record header + bump scratch_cursor ---
        self.builder.position_at_end(read_ok_bb);
        let nread = self
            .builder
            .build_load(i32_t, nread_slot, &self.next_name("rf_nread_val"))
            .map_err(|e| LlvmError::Codegen(format!("read_file nread load: {e}")))?
            .into_int_value();
        // Write `nread` into the record's `[len]` header.
        let record_hdr_ptr = self.arena_addr_i32(record_off)?;
        self.builder
            .build_store(record_hdr_ptr, nread)
            .map_err(|e| LlvmError::Codegen(format!("read_file record len store: {e}")))?;
        // scratch_cursor = (record_off - scratch_base) + 4 + nread.
        let rec_rel = self
            .builder
            .build_int_sub(record_off, scratch_base, &self.next_name("rf_rec_rel"))
            .map_err(|e| LlvmError::Codegen(format!("read_file rec rel: {e}")))?;
        let four_plus = self
            .builder
            .build_int_add(
                nread,
                i32_t.const_int(4, false),
                &self.next_name("rf_four_plus"),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file 4+nread: {e}")))?;
        let new_cursor = self
            .builder
            .build_int_add(rec_rel, four_plus, &self.next_name("rf_new_cursor"))
            .map_err(|e| LlvmError::Codegen(format!("read_file new cursor: {e}")))?;
        self.builder
            .build_store(cursor_gep, new_cursor)
            .map_err(|e| LlvmError::Codegen(format!("read_file cursor store: {e}")))?;
        self.builder
            .build_unconditional_branch(cont_bb)
            .map_err(|e| LlvmError::Codegen(format!("read_file ok branch: {e}")))?;

        // --- trap: record CapabilityDenied + route to trap epilogue ---
        self.builder.position_at_end(trap_bb);
        let trap_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TRAP_CODE), false)],
                    &self.next_name("rf_wasm_trap_gep"),
                )
                .map_err(|e| LlvmError::Codegen(format!("read_file wasm trap gep: {e}")))?
        };
        self.builder
            .build_store(
                trap_gep,
                i64_t.const_int(crate::state::NativeTrap::CapabilityDenied as u64, false),
            )
            .map_err(|e| LlvmError::Codegen(format!("read_file wasm trap store: {e}")))?;
        self.emit_trap_sentinel_return("ReadFile")?;

        // --- cont: push the record offset as a String operand ---
        self.builder.position_at_end(cont_bb);
        self.push(record_off, IrType::String);
        Ok(())
    }

    /// Allocate an entry-block slot of `ty` (a linear-memory stack slot
    /// on wasm32) and return its pointer. Used for the WASI marshalling
    /// scratch (fd_out / iovec / nread) the fd protocol writes through.
    fn alloc_entry(
        &mut self,
        ty: inkwell::types::BasicTypeEnum<'ctx>,
        hint: &str,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let entry_bb = self
            .func
            .get_first_basic_block()
            .ok_or_else(|| LlvmError::Codegen("wasi cap: function has no entry block".into()))?;
        let cur = self.builder.get_insert_block();
        if let Some(first) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let slot = self
            .builder
            .build_alloca(ty, &self.next_name(hint))
            .map_err(|e| LlvmError::Codegen(format!("wasi cap {hint} alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }
        Ok(slot)
    }

    /// Native: declare the host `extern "C" fn() -> i64` helper symbol
    /// and emit a `call`. The MCJIT engine maps the symbol to the host
    /// fn address (`crate::evaluator`).
    fn emit_read_native_helper(&mut self, symbol: &str, hint: &str) -> Result<(), LlvmError> {
        let i64_t = self.ctx.i64_type();
        let helper = match self.module.get_function(symbol) {
            Some(f) => f,
            None => {
                let fn_ty = i64_t.fn_type(&[], false);
                self.module
                    .add_function(symbol, fn_ty, Some(inkwell::module::Linkage::External))
            }
        };
        let call_site = self
            .builder
            .build_call(helper, &[], &self.next_name(hint))
            .map_err(|e| LlvmError::Codegen(format!("{hint} build_call: {e}")))?;
        let result = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "{hint} helper returned {other:?}, expected i64"
                )));
            }
        };
        self.push(result, IrType::I64);
        Ok(())
    }

    /// Allocate an 8-byte linear-memory scratch slot in the entry block
    /// and return both the pointer and its i32 linear-memory address.
    /// The `alloca` lives in the entry block so mem2reg / SROA can keep
    /// the surrounding code tight; on wasm32 it is a stack slot in the
    /// module's linear memory.
    fn alloc_scratch8(
        &mut self,
    ) -> Result<(PointerValue<'ctx>, inkwell::values::IntValue<'ctx>), LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let entry_bb = self
            .func
            .get_first_basic_block()
            .ok_or_else(|| LlvmError::Codegen("wasi cap: function has no entry block".into()))?;
        let cur = self.builder.get_insert_block();
        if let Some(first) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let slot = self
            .builder
            .build_alloca(i64_t, &self.next_name("wasi_scratch"))
            .map_err(|e| LlvmError::Codegen(format!("wasi cap scratch alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }
        // Linear-memory address (i32) of the scratch slot for the WASI
        // ABI pointer operand.
        let addr_i32 = self
            .builder
            .build_ptr_to_int(slot, i32_t, &self.next_name("wasi_scratch_addr"))
            .map_err(|e| LlvmError::Codegen(format!("wasi cap scratch ptrtoint: {e}")))?;
        Ok((slot, addr_i32))
    }

    /// Declare a standard-WASI import: retarget the undefined external
    /// off the default `env` module onto `wasi_snapshot_preview1` via
    /// the LLVM `wasm-import-module` / `wasm-import-name` attributes.
    fn declare_wasi_import(
        &self,
        name: &str,
        fn_ty: inkwell::types::FunctionType<'ctx>,
    ) -> inkwell::values::FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let f = self
            .module
            .add_function(name, fn_ty, Some(inkwell::module::Linkage::External));
        f.add_attribute(
            inkwell::attributes::AttributeLoc::Function,
            self.ctx
                .create_string_attribute("wasm-import-module", WASI_MODULE),
        );
        f.add_attribute(
            inkwell::attributes::AttributeLoc::Function,
            self.ctx.create_string_attribute("wasm-import-name", name),
        );
        f
    }

    /// Load the `u64` the WASI host wrote into the scratch slot and push
    /// it as an `Int`. `errno` is ignored: a standard WASI host returns
    /// `0` for the supported clock / random calls, and a non-zero errno
    /// leaves the (zero-initialised-by-alloca) slot at a well-defined
    /// value. The marshalling proof lives in the test (the host writes
    /// a real reading, the guest reads it back).
    fn push_scratch_u64(&mut self, slot: PointerValue<'ctx>, hint: &str) -> Result<(), LlvmError> {
        let i64_t = self.ctx.i64_type();
        let v = self
            .builder
            .build_load(i64_t, slot, &self.next_name(hint))
            .map_err(|e| LlvmError::Codegen(format!("{hint} scratch load: {e}")))?
            .into_int_value();
        self.push(v, IrType::I64);
        Ok(())
    }

    /// wasm32 `clock()` → standard WASI `clock_time_get`.
    fn emit_read_clock_wasi(&mut self) -> Result<(), LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        // (i32 clock_id, i64 precision, i32 *time) -> i32 errno
        let fn_ty = i32_t.fn_type(&[i32_t.into(), i64_t.into(), ptr_t.into()], false);
        let import = self.declare_wasi_import("clock_time_get", fn_ty);

        let (slot, addr_i32) = self.alloc_scratch8()?;
        // The opaque `ptr` operand: rebuild a pointer from the i32 addr
        // so the WASI ABI sees a linear-memory pointer. (On wasm32 the
        // ptr IS the i32 address; the round-trip keeps the type system
        // happy and is folded away.)
        let scratch_ptr = self
            .builder
            .build_int_to_ptr(addr_i32, ptr_t, &self.next_name("clock_scratch_ptr"))
            .map_err(|e| LlvmError::Codegen(format!("clock scratch inttoptr: {e}")))?;
        let args: [BasicMetadataValueEnum<'ctx>; 3] = [
            i32_t.const_int(WASI_CLOCK_REALTIME, false).into(),
            i64_t.const_zero().into(),
            scratch_ptr.into(),
        ];
        self.builder
            .build_call(import, &args, &self.next_name("clock_time_get"))
            .map_err(|e| LlvmError::Codegen(format!("clock_time_get call: {e}")))?;
        self.push_scratch_u64(slot, "clock_ns")
    }

    /// wasm32 `random()` → standard WASI `random_get`.
    fn emit_read_random_wasi(&mut self) -> Result<(), LlvmError> {
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        // (i32 *buf, i32 len) -> i32 errno
        let fn_ty = i32_t.fn_type(&[ptr_t.into(), i32_t.into()], false);
        let import = self.declare_wasi_import("random_get", fn_ty);

        let (slot, addr_i32) = self.alloc_scratch8()?;
        let buf_ptr = self
            .builder
            .build_int_to_ptr(addr_i32, ptr_t, &self.next_name("rand_buf_ptr"))
            .map_err(|e| LlvmError::Codegen(format!("random buf inttoptr: {e}")))?;
        let args: [BasicMetadataValueEnum<'ctx>; 2] =
            [buf_ptr.into(), i32_t.const_int(8, false).into()];
        self.builder
            .build_call(import, &args, &self.next_name("random_get"))
            .map_err(|e| LlvmError::Codegen(format!("random_get call: {e}")))?;
        self.push_scratch_u64(slot, "random_bits")
    }
}
