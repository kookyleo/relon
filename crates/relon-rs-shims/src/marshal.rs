//! Buffer-protocol marshalling helper used by the build.rs-generated
//! bindings.
//!
//! The generated binding for a buffer-protocol entry boils down to:
//!
//! ```ignore
//! pub fn main(state: &SandboxState, s: &str) -> i64 {
//!     let args = [ArgValue::String(s)];
//!     match relon_rs_shims::call_buffer_entry(
//!         JIT_ENTRY,
//!         CONST_DATA,
//!         MAIN_FIELDS,
//!         MAIN_ROOT_SIZE,
//!         RETURN_FIELDS,
//!         RETURN_ROOT_SIZE,
//!         RETURN_HAS_TAIL,
//!         &args,
//!     ).expect("relon AOT body trapped") {
//!         RetValue::Int(v) => v,
//!         other => unreachable!("compile-time type mismatch: {other:?}"),
//!     }
//! }
//! ```
//!
//! All the heavy lifting (arena allocation, `BufferBuilder` packing,
//! JIT dispatch with the canonical buffer-protocol signature, output
//! decode) lives in [`call_buffer_entry`]. The binding is reduced to a
//! couple of `match` arms over the typed Rust args / return.
//!
//! ## Why constants for `EmittedField`?
//!
//! The binding embeds the per-field metadata (name, offset, type tag)
//! as `const` slices instead of round-tripping through serde at start-
//! up. The schema is known at build time and never changes between
//! runs — paying a per-call `HashMap` rebuild would defeat the
//! AOT-link win. The `BufferBuilder` / `BufferReader` instances inside
//! [`call_buffer_entry`] reconstruct the canonical `Schema` /
//! `OffsetTable` from the binding's `EmittedField` slice on every
//! call, which is cheap (a couple of dozen pointer writes per field).

use core::cell::RefCell;

use relon_eval_api::buffer::{BufferBuilder, BufferReader};
use relon_eval_api::layout::{FieldKind, FieldOffset, ListElementKind, OffsetTable};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};

use crate::sandbox_state::{ArenaState, SandboxState};

/// Per-field metadata the build.rs side stamps into the generated
/// binding as a `const` slice. Mirrors the build-side
/// `relon_codegen_llvm::EmittedField` type so the binding can
/// initialise its static tables without depending on the codegen
/// crate at runtime.
#[derive(Debug, Clone, Copy)]
pub struct EmittedField {
    /// Field name as declared in the `.relon` source.
    pub name: &'static str,
    /// Byte offset of the field's fixed-area slot inside the enclosing
    /// record. Pre-computed by `relon-eval-api::layout::SchemaLayout`
    /// at build time.
    pub offset: u32,
    /// Erased canonical type tag — drives `BufferBuilder` /
    /// `BufferReader` dispatch + the binding's Rust-side `match`.
    pub ty: EmittedFieldType,
}

/// Phase 2 supported leaf-type set. Mirrors
/// `relon_codegen_llvm::EmittedFieldType`. `Float` / `List*` / `Schema` /
/// `Closure` surface as `UnsupportedSignature` on the build.rs side
/// before they can reach the binding, so the binding never sees an
/// unknown tag.
///
/// ## Three-crate triple contract
///
/// This enum is the runtime mirror of
/// `relon_codegen_llvm::EmittedFieldType`. The tag must stay
/// byte-for-byte identical across three crates (codegen-llvm, this
/// runtime shim, build generator) — see the codegen-llvm enum's docs
/// for the master contract. **Adding a variant here means**: (1) add
/// the matching [`ArgValue`] / [`RetValue`] variant; (2) add the
/// `pack_<type>` / `unpack_<type>` sibling helper used by
/// [`call_buffer_entry`] + extend [`ty_to_repr`] / [`synthesise_layout`];
/// (3) widen codegen-llvm's `emitted_field_type_for` + the build
/// generator's `rust_type_for`; (4) extend the cross-crate round-trip
/// guard test (`relon-rs-build/tests/marshal_roundtrip.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedFieldType {
    /// `i64` (Inline 8/8).
    Int,
    /// `f64` (Inline 8/8, 8 LE bytes IEEE-754).
    Float,
    /// `bool` (Inline 1/1).
    Bool,
    /// `()` (Inline 1/1, always zero).
    Null,
    /// `&str` / `String` — pointer-indirect, fixed slot is a 4-byte
    /// buffer-relative offset to a `[len: u32 LE][utf8 bytes]` tail
    /// record.
    String,
    /// `&[i64]` / `Vec<i64>` — pointer-indirect like `String`; fixed slot
    /// is a 4-byte buffer-relative offset to a `[len: u32 LE][pad to 8]
    /// [i64 LE …]` tail record (8/8-inline elements).
    ListInt,
}

/// Argument value the binding hands to [`call_buffer_entry`]. The
/// declaration order must match the `#main(...)` parameter order
/// recorded in the binding's `MAIN_FIELDS` table.
///
/// One variant per [`EmittedFieldType`] tag — adding a leaf type adds a
/// variant here and a `pack_<type>` helper (see the
/// [`EmittedFieldType`] triple contract).
#[derive(Debug)]
pub enum ArgValue<'a> {
    /// `i64` argument bound to an `EmittedFieldType::Int` slot.
    Int(i64),
    /// `f64` argument bound to an `EmittedFieldType::Float` slot.
    Float(f64),
    /// `bool` argument bound to an `EmittedFieldType::Bool` slot.
    Bool(bool),
    /// `Null` argument bound to an `EmittedFieldType::Null` slot.
    Null,
    /// `&str` argument bound to an `EmittedFieldType::String` slot.
    /// The buffer writer copies the bytes into the arena's tail
    /// region — no caller-side aliasing constraint beyond `'a >`
    /// the call duration.
    String(&'a str),
    /// `&[i64]` argument bound to an `EmittedFieldType::ListInt` slot.
    /// The buffer writer copies the elements into the arena's tail
    /// region as a `[len][i64…]` record.
    ListInt(&'a [i64]),
}

/// Return value decoded from the JIT entry's output buffer. The
/// binding's outer wrapper matches on the variant matching its
/// declared `#main` return type and unwraps the payload.
///
/// One variant per [`EmittedFieldType`] tag — adding a leaf type adds a
/// variant here and an `unpack_<type>` helper (see the
/// [`EmittedFieldType`] triple contract).
#[derive(Debug, Clone, PartialEq)]
pub enum RetValue {
    /// `i64` return decoded from an `EmittedFieldType::Int` slot.
    Int(i64),
    /// `f64` return decoded from an `EmittedFieldType::Float` slot.
    Float(f64),
    /// `bool` return decoded from an `EmittedFieldType::Bool` slot.
    Bool(bool),
    /// `Null` return — present so a `#main(...) -> Null` shape
    /// round-trips through the marshalling helper without special-
    /// casing.
    Null,
    /// `String` return decoded from an `EmittedFieldType::String`
    /// slot. The bytes are copied out of the arena before the per-
    /// call buffer is recycled, so the caller can keep the value
    /// after the dispatch returns.
    String(String),
    /// `Vec<i64>` return decoded from an `EmittedFieldType::ListInt`
    /// slot. The elements are copied out of the arena's tail record
    /// before the per-call buffer is recycled.
    ListInt(Vec<i64>),
}

/// Errors surfaced by [`call_buffer_entry`]. Phase 2 keeps the surface
/// small — wider trap propagation (timeout, OOM, capability denial,
/// user-raised errors) lands with Phase 3.
#[derive(Debug)]
pub enum BufferEntryError {
    /// Argument list length didn't match the binding's declared
    /// `#main` arity. Always an internal binding bug — the
    /// build.rs-generated wrapper is supposed to count its args
    /// before dispatch.
    Arity {
        /// Expected `#main` arity.
        expected: usize,
        /// Actual length of the `args` slice.
        actual: usize,
    },
    /// Argument type didn't match the binding's declared field type.
    /// Same root cause as `Arity` — a binding bug.
    TypeMismatch {
        /// 0-based index of the mismatched arg in declaration order.
        index: usize,
        /// Declared type the binding expected.
        expected: EmittedFieldType,
        /// Actual variant the binding passed.
        actual: &'static str,
    },
    /// The `BufferBuilder` / `BufferReader` rejected the marshalling
    /// (slot offset / type mismatch the binding couldn't catch
    /// up-front — e.g. a value larger than `u32::MAX`).
    Buffer(String),
    /// The JIT entry returned a negative `bytes_written` — surfaces a
    /// host-side trap the JIT raised (today: only `llvm.trap` on
    /// arithmetic UB; Phase 3 adds richer trap codes).
    NegativeBytesWritten(i32),
    /// A gated `#native` call was denied: the JIT body's `Op::CheckCap`
    /// gate found the required [`relon_eval_api::CapabilityBit`] clear
    /// in the granted `caps` mask and trapped. The body recorded
    /// `NativeTrap::CapabilityDenied` (= 3) in `ArenaState::trap_code`
    /// and returned the negative `bytes_written` sentinel; the marshaller
    /// reads the code back and lifts it here instead of letting the
    /// negative sentinel surface as an opaque `NegativeBytesWritten`.
    /// The host grants the capability by threading a populated
    /// [`crate::SandboxState`] (see [`crate::SandboxState::grant`]).
    CapabilityDenied,
}

impl core::fmt::Display for BufferEntryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Arity { expected, actual } => write!(
                f,
                "relon-rs-shims: #main expects {expected} arg(s), binding passed {actual}"
            ),
            Self::TypeMismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "relon-rs-shims: arg #{index} expects {expected:?}, binding passed {actual}"
            ),
            Self::Buffer(msg) => write!(f, "relon-rs-shims: buffer marshalling failed: {msg}"),
            Self::NegativeBytesWritten(code) => write!(
                f,
                "relon-rs-shims: JIT entry returned trap code {code} (negative bytes_written)"
            ),
            Self::CapabilityDenied => write!(
                f,
                "relon-rs-shims: gated `#native` call denied by the capability gate \
                 (grant the capability via SandboxState::with_capabilities / ::grant)"
            ),
        }
    }
}

/// Trap code the LLVM emitter stores in `ArenaState::trap_code` when a
/// `#native` body's `Op::CheckCap` gate denies a call. Mirror of
/// `relon_codegen_llvm::state::NativeTrap::CapabilityDenied as u64`
/// (and the cranelift `TrapKind::CapabilityDenied`) — the value is
/// stable across backends (= 3). Duplicated here rather than imported
/// so the runtime shim doesn't take a dep on the LLVM codegen crate.
const NATIVE_TRAP_CAPABILITY_DENIED: u64 = 3;

impl std::error::Error for BufferEntryError {}

/// Buffer-protocol JIT entry C ABI signature. Build.rs hands this in
/// as a transmuted fn-pointer from the `extern "C"` declaration the
/// binding inserts.
pub type BufferEntryFn = unsafe extern "C" fn(*const ArenaState, i32, i32, i32, i32, i64) -> i32;

thread_local! {
    /// Per-thread arena pool. Mirrors the LLVM-side
    /// `LLVM_ARENA_POOL` so steady-state dispatches reuse the
    /// allocation across calls.
    static SHIM_ARENA_POOL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Pack `args` into the arena, dispatch through `entry`, decode the
/// return record. Used by every buffer-protocol binding the build.rs
/// side generates.
///
/// The shape mirrors `relon_codegen_llvm::evaluator::run_main_buffer`.
/// The AOT-linked entry uses the same JIT body, so the arena layout
/// and dispatch protocol must match byte-for-byte. The duplication is
/// deliberate: the LLVM crate isn't a runtime dep of `relon-rs-shims`
/// (see the crate-level docs for the rationale).
#[allow(clippy::too_many_arguments)]
pub fn call_buffer_entry(
    entry: BufferEntryFn,
    const_data: &[u8],
    main_fields: &[EmittedField],
    main_root_size: u32,
    return_fields: &[EmittedField],
    return_root_size: u32,
    return_has_tail: bool,
    _state: &SandboxState,
    args: &[ArgValue<'_>],
) -> Result<Vec<RetValue>, BufferEntryError> {
    if args.len() != main_fields.len() {
        return Err(BufferEntryError::Arity {
            expected: main_fields.len(),
            actual: args.len(),
        });
    }

    // 1. Pack the input buffer. We reconstruct the canonical Schema +
    // OffsetTable from the per-field metadata the binding handed in.
    // The reconstruction is cheap (linear in the field count) and
    // saves the binding from depending on `relon-eval-api`
    // transitively.
    let main_schema = synthesise_schema(main_fields, "MainParams");
    let main_layout = synthesise_layout(main_fields, main_root_size);
    let mut builder = BufferBuilder::new(&main_layout, &main_schema.fields);
    for (i, (field, arg)) in main_fields.iter().zip(args.iter()).enumerate() {
        pack_arg(&mut builder, i, field, arg)?;
    }
    let in_bytes = builder.finish();

    // 2. Lay out the arena. Identical to
    // `relon_codegen_llvm::evaluator::dispatch_with_arena` — the JIT
    // body's address arithmetic is calibrated against this layout.
    let in_len = in_bytes.len() as u32;
    let out_root_size = return_root_size;
    let tail_cap: u32 = if return_has_tail { 65_536 } else { 0 };
    let out_cap = relon_util::align_up(out_root_size.max(8) + tail_cap + 16, 8);
    let const_data_len = u32::try_from(const_data.len())
        .map_err(|_| BufferEntryError::Buffer("const-data section exceeds u32 range".into()))?;
    let in_ptr = relon_util::align_up(const_data_len, 8);
    let out_ptr = relon_util::align_up(in_ptr + in_len, 8);
    let scratch_base = relon_util::align_up(out_ptr + out_cap, 8);
    // 1 MiB scratch matches the LLVM evaluator's figure.
    let scratch_size: u32 = 1_048_576;
    let arena_size = (scratch_base + scratch_size) as usize;

    // The granted capability bitmask the host threaded through
    // `SandboxState`. Forwarded verbatim as the entry's `caps`
    // argument so a gated `#native` body's `Op::CheckCap` gate consults
    // the host's actual grant (was hard-coded `0`, which denied every
    // gated call regardless of the host's posture).
    let caps_mask = _state.caps_mask();

    // 3. Acquire the per-thread arena pool, dispatch, decode.
    SHIM_ARENA_POOL.with(|cell| match cell.try_borrow_mut() {
        Ok(mut buf) => dispatch_with_arena(
            entry,
            const_data,
            &mut buf,
            arena_size,
            in_ptr,
            in_len,
            out_ptr,
            out_cap,
            scratch_base,
            caps_mask,
            &in_bytes,
            return_fields,
            return_root_size,
        ),
        Err(_) => {
            // Reentrant call (the JIT body looped back through the
            // entry on the same thread). Fall back to a fresh
            // `Vec<u8>` — correctness over pool reuse on the
            // vanishingly rare path.
            let mut fallback: Vec<u8> = Vec::new();
            dispatch_with_arena(
                entry,
                const_data,
                &mut fallback,
                arena_size,
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                scratch_base,
                caps_mask,
                &in_bytes,
                return_fields,
                return_root_size,
            )
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn dispatch_with_arena(
    entry: BufferEntryFn,
    const_data: &[u8],
    arena: &mut Vec<u8>,
    arena_size: usize,
    in_ptr: u32,
    in_len: u32,
    out_ptr: u32,
    out_cap: u32,
    scratch_base: u32,
    caps_mask: i64,
    in_bytes: &[u8],
    return_fields: &[EmittedField],
    return_root_size: u32,
) -> Result<Vec<RetValue>, BufferEntryError> {
    if arena.len() < arena_size {
        arena.resize(arena_size, 0);
    }
    let observable_end = (out_ptr + out_cap) as usize;
    debug_assert!(observable_end <= arena_size);
    debug_assert!(const_data.len() <= in_ptr as usize);
    arena[const_data.len()..observable_end].fill(0);
    if !const_data.is_empty() {
        arena[..const_data.len()].copy_from_slice(const_data);
    }
    arena[in_ptr as usize..in_ptr as usize + in_bytes.len()].copy_from_slice(in_bytes);

    let live_arena = &mut arena[..arena_size];
    let state = ArenaState::new(live_arena, scratch_base);
    let state_ptr: *const ArenaState = &state;

    // SAFETY: the JIT entry was emitted with the canonical buffer-
    // protocol signature (`relon_codegen_llvm::emitter::emit_module_funcs`).
    // The arena outlives the call — `state_ptr` is borrowed for the
    // duration of `f(...)` only. We wrap in `catch_unwind` so a JIT-
    // side `llvm.trap` (lowered to a Rust panic by the panic runtime)
    // surfaces as a typed error rather than unwinding past the FFI
    // boundary.
    let bytes_written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        entry(
            state_ptr,
            in_ptr as i32,
            in_len as i32,
            out_ptr as i32,
            out_cap as i32,
            caps_mask,
        )
    }))
    .map_err(|_| BufferEntryError::NegativeBytesWritten(-1))?;

    if bytes_written < 0 {
        // A negative `bytes_written` is the trap sentinel: the JIT body
        // recorded a cause in `ArenaState::trap_code` and bailed. Read
        // it back to lift a typed error. Today the only code the
        // buffer-protocol body raises through this path is
        // `CapabilityDenied` (a gated `#native` call the granted `caps`
        // mask didn't authorise); any other / absent code keeps the
        // opaque `NegativeBytesWritten` surface.
        return match state.trap_code() {
            NATIVE_TRAP_CAPABILITY_DENIED => Err(BufferEntryError::CapabilityDenied),
            _ => Err(BufferEntryError::NegativeBytesWritten(bytes_written)),
        };
    }
    let bw = bytes_written as usize;

    let read_len = bw.max(return_root_size as usize);
    let read_end = out_ptr as usize + read_len;
    if read_end > arena_size {
        return Err(BufferEntryError::Buffer(format!(
            "arena too small for return decode: need {read_end}, have {arena_size}"
        )));
    }
    let return_schema = synthesise_schema(return_fields, "Ret");
    let return_layout = synthesise_layout(return_fields, return_root_size);
    let out_bytes = &arena[out_ptr as usize..read_end];
    let reader = BufferReader::new(&return_layout, &return_schema.fields, out_bytes)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))?;
    let mut out = Vec::with_capacity(return_fields.len());
    for field in return_fields.iter() {
        out.push(unpack_ret(&reader, field)?);
    }
    Ok(out)
}

// --- per-variant marshalling helpers (S1.A seam) ---
//
// `call_buffer_entry` delegates per-field pack / unpack to one helper
// per [`EmittedFieldType`] tag. A future Float / List lane adds its
// `pack_<type>` / `unpack_<type>` sibling here without disturbing the
// others — mirroring the codegen-llvm `marshal_<type>_in` / `_out`
// seam.

/// Pack one typed [`ArgValue`] into `builder` for `field`'s slot,
/// dispatching on the `(field.ty, arg)` pairing. A tag/value mismatch
/// surfaces as [`BufferEntryError::TypeMismatch`] (a binding bug).
fn pack_arg(
    builder: &mut BufferBuilder<'_>,
    index: usize,
    field: &EmittedField,
    arg: &ArgValue<'_>,
) -> Result<(), BufferEntryError> {
    match (field.ty, arg) {
        (EmittedFieldType::Int, ArgValue::Int(v)) => pack_int(builder, field.name, *v),
        (EmittedFieldType::Float, ArgValue::Float(v)) => pack_float(builder, field.name, *v),
        (EmittedFieldType::Bool, ArgValue::Bool(v)) => pack_bool(builder, field.name, *v),
        (EmittedFieldType::Null, ArgValue::Null) => pack_null(builder, field.name),
        (EmittedFieldType::String, ArgValue::String(s)) => pack_string(builder, field.name, s),
        (EmittedFieldType::ListInt, ArgValue::ListInt(v)) => pack_list_int(builder, field.name, v),
        // ----- add new leaf pack arm above this line -----
        (expected, actual) => Err(BufferEntryError::TypeMismatch {
            index,
            expected,
            actual: arg_variant_name(actual),
        }),
    }
}

fn pack_int(builder: &mut BufferBuilder<'_>, name: &str, v: i64) -> Result<(), BufferEntryError> {
    builder
        .write_int(name, v)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn pack_float(builder: &mut BufferBuilder<'_>, name: &str, v: f64) -> Result<(), BufferEntryError> {
    builder
        .write_float(name, v)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn pack_bool(builder: &mut BufferBuilder<'_>, name: &str, v: bool) -> Result<(), BufferEntryError> {
    builder
        .write_bool(name, v)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn pack_null(builder: &mut BufferBuilder<'_>, name: &str) -> Result<(), BufferEntryError> {
    builder
        .write_null(name)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn pack_string(
    builder: &mut BufferBuilder<'_>,
    name: &str,
    v: &str,
) -> Result<(), BufferEntryError> {
    builder
        .write_string(name, v)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn pack_list_int(
    builder: &mut BufferBuilder<'_>,
    name: &str,
    v: &[i64],
) -> Result<(), BufferEntryError> {
    builder
        .write_list_int(name, v)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

/// Decode one `field` slot out of `reader` into the matching
/// [`RetValue`] variant.
fn unpack_ret(
    reader: &BufferReader<'_>,
    field: &EmittedField,
) -> Result<RetValue, BufferEntryError> {
    match field.ty {
        EmittedFieldType::Int => unpack_int(reader, field.name),
        EmittedFieldType::Float => unpack_float(reader, field.name),
        EmittedFieldType::Bool => unpack_bool(reader, field.name),
        EmittedFieldType::Null => unpack_null(reader, field.name),
        EmittedFieldType::String => unpack_string(reader, field.name),
        EmittedFieldType::ListInt => unpack_list_int(reader, field.name),
        // ----- add new leaf unpack arm above this line -----
    }
}

fn unpack_int(reader: &BufferReader<'_>, name: &str) -> Result<RetValue, BufferEntryError> {
    reader
        .read_int(name)
        .map(RetValue::Int)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn unpack_float(reader: &BufferReader<'_>, name: &str) -> Result<RetValue, BufferEntryError> {
    reader
        .read_float(name)
        .map(RetValue::Float)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn unpack_bool(reader: &BufferReader<'_>, name: &str) -> Result<RetValue, BufferEntryError> {
    reader
        .read_bool(name)
        .map(RetValue::Bool)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn unpack_null(reader: &BufferReader<'_>, name: &str) -> Result<RetValue, BufferEntryError> {
    reader
        .read_null(name)
        .map(|_| RetValue::Null)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn unpack_string(reader: &BufferReader<'_>, name: &str) -> Result<RetValue, BufferEntryError> {
    reader
        .read_string(name)
        .map(|s| RetValue::String(s.to_owned()))
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn unpack_list_int(reader: &BufferReader<'_>, name: &str) -> Result<RetValue, BufferEntryError> {
    reader
        .read_list_int(name)
        .map(RetValue::ListInt)
        .map_err(|e| BufferEntryError::Buffer(format!("{e}")))
}

fn arg_variant_name(arg: &ArgValue<'_>) -> &'static str {
    match arg {
        ArgValue::Int(_) => "Int",
        ArgValue::Float(_) => "Float",
        ArgValue::Bool(_) => "Bool",
        ArgValue::Null => "Null",
        ArgValue::String(_) => "String",
        ArgValue::ListInt(_) => "ListInt",
    }
}

fn ty_to_repr(ty: EmittedFieldType) -> TypeRepr {
    match ty {
        EmittedFieldType::Int => TypeRepr::Int,
        EmittedFieldType::Float => TypeRepr::Float,
        EmittedFieldType::Bool => TypeRepr::Bool,
        EmittedFieldType::Null => TypeRepr::Null,
        EmittedFieldType::String => TypeRepr::String,
        EmittedFieldType::ListInt => TypeRepr::List {
            element: Box::new(TypeRepr::Int),
        },
    }
}

fn synthesise_schema(fields: &[EmittedField], name: &str) -> Schema {
    Schema {
        name: name.to_string(),
        generics: Vec::new(),
        fields: fields
            .iter()
            .map(|f| Field {
                name: f.name.to_string(),
                ty: ty_to_repr(f.ty),
                default: None,
            })
            .collect(),
        is_tuple: false,
    }
}

fn synthesise_layout(fields: &[EmittedField], root_size: u32) -> OffsetTable {
    // Rebuild the slot table the LLVM emitter consumed at build time.
    // Each entry mirrors what `relon-eval-api::layout::SchemaLayout`
    // would have produced for the same field — the kind / size / align
    // sidecar is the per-type-tag boilerplate the writer dispatches
    // on. We don't re-derive `root_size` here; the binding stamps the
    // exact value the codegen used so the writer's fixed-area bookkeeping
    // matches the JIT body's expectations.
    let mut out = OffsetTable {
        fields: Vec::with_capacity(fields.len()),
        root_size: root_size as usize,
        root_align: 8,
    };
    for f in fields {
        let (size, align, kind, list_element) = match f.ty {
            EmittedFieldType::Int => (8, 8, FieldKind::Inline { size: 8, align: 8 }, None),
            EmittedFieldType::Float => (8, 8, FieldKind::Inline { size: 8, align: 8 }, None),
            EmittedFieldType::Bool => (1, 1, FieldKind::Inline { size: 1, align: 1 }, None),
            EmittedFieldType::Null => (1, 1, FieldKind::Inline { size: 1, align: 1 }, None),
            EmittedFieldType::String => {
                (4, 4, FieldKind::PointerIndirect { tail_alignment: 1 }, None)
            }
            // `List<Int>` mirrors `relon-eval-api::layout`'s
            // `list_layout_decision` Int arm byte-for-byte: a 4/4
            // pointer slot, an 8-aligned tail record, and 8/8-inline
            // i64 elements.
            EmittedFieldType::ListInt => (
                4,
                4,
                FieldKind::PointerIndirect { tail_alignment: 8 },
                Some(ListElementKind::InlineFixed {
                    elem_size: 8,
                    elem_align: 8,
                }),
            ),
        };
        out.fields.push(FieldOffset {
            name: f.name.to_string(),
            offset: f.offset as usize,
            size,
            align,
            kind,
            list_element,
        });
    }
    out
}
