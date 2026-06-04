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
    /// P-fs Stage 1 honest scope: NOT YET IMPLEMENTED on wasm32. The
    /// spike (`tests/aot_wasm_wasi_fs_spike.rs`) proved the fd protocol +
    /// preopened-dir host in isolation, but productionizing it here means
    /// reconciling the WASI linear-memory iovec marshalling with the
    /// LLVM buffer-protocol's `out_ptr`-relative tail-record convention
    /// (`emit_store_field_pointer_indirect`) and the harness readback —
    /// a larger block than the native arms. Surfacing a loud codegen
    /// error keeps the wasm path from emitting a silently-wrong module
    /// (no fake green): a `read_file` program simply refuses to compile
    /// for wasm32 until the marshalling is wired.
    fn emit_read_file_wasi(&mut self) -> Result<(), LlvmError> {
        Err(LlvmError::Codegen(
            "Op::ReadFile (read_file) wasm32 lowering not yet implemented (P-fs Stage 1 ships \
             tree-walk + cranelift + llvm-native; wasm fd-protocol marshalling is deferred — \
             see crates/relon-codegen-llvm/src/codegen/wasi_cap.rs)"
                .into(),
        ))
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
