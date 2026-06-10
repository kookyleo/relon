//! Shared `f64 → String` formatting core for `Op::FloatToStr`.
//!
//! Every compiled backend (cranelift-AOT, LLVM-native, LLVM-wasm via
//! the parity-test host import) funnels its Float rendering through
//! [`format_f64_display`] so the produced bytes are the *same Rust
//! `Display` bytes* the tree-walk oracle emits for `Value::Float`
//! (`write!(f, "{}", fl)` over `OrderedFloat<f64>`, which forwards to
//! the plain `f64` `Display`).
//!
//! Properties of Rust's `f64` `Display` this op inherits (and the
//! boundary tests pin):
//!
//! * shortest round-trip decimal, positional notation only — never
//!   scientific (`1e300` renders as a 301-char digit string);
//! * integral values drop the fraction: `1.0 → "1"`, `-0.0 → "-0"`;
//! * `NaN → "NaN"`, `inf → "inf"`, `-inf → "-inf"`;
//! * the longest possible rendering is the negative minimum subnormal
//!   `-5e-324` at 327 bytes ("-0." + 324 fraction digits), which is
//!   why [`FLOAT_TO_STR_MAX_PAYLOAD`] is 352 (327 rounded up with
//!   margin) and the scratch record allocation is the comfortably
//!   larger [`FLOAT_TO_STR_RECORD_SIZE`].
//!
//! The function takes the IEEE-754 **bit pattern** rather than an
//! `f64` so the FFI boundary (cranelift vtable helper / LLVM extern
//! shim / wasm import) can carry the value in an integer register
//! without any backend-specific float-ABI concern — both compiled
//! value models already ride F64 values as i64 bits at call edges.

use core::fmt::Write;

/// Upper bound on the rendered payload length in bytes. The true
/// maximum for `f64` `Display` is 327 (`-5e-324`); 352 leaves margin
/// and keeps the constant audit-friendly (`327 < 352`, asserted in
/// tests).
pub const FLOAT_TO_STR_MAX_PAYLOAD: usize = 352;

/// Scratch-arena record allocation for one `FloatToStr` result:
/// `[len: u32 LE][utf8 payload]` rounded up generously. 768 ≥ 4 +
/// [`FLOAT_TO_STR_MAX_PAYLOAD`] with ample headroom, and is already
/// 4-byte aligned (the arena record alignment unit).
pub const FLOAT_TO_STR_RECORD_SIZE: u32 = 768;

/// Bounded `fmt::Write` sink over a byte slice. Fails (instead of
/// panicking or truncating) if the formatted output would overflow
/// the buffer — the callers translate that into a loud trap, never a
/// silently clipped string.
struct SliceWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = self.len.checked_add(bytes.len()).ok_or(core::fmt::Error)?;
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// Render the `f64` whose IEEE-754 bit pattern is `bits` into `out`
/// using Rust's `f64` `Display` (the tree-walk oracle's exact byte
/// producer). Returns the payload length written at `out[..len]`, or
/// `None` if `out` is too small (callers allocate
/// ≥ [`FLOAT_TO_STR_MAX_PAYLOAD`], so `None` indicates a caller bug
/// and must surface as a loud failure).
pub fn format_f64_display(bits: u64, out: &mut [u8]) -> Option<usize> {
    let v = f64::from_bits(bits);
    let mut w = SliceWriter { buf: out, len: 0 };
    write!(w, "{v}").ok()?;
    Some(w.len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(v: f64) -> String {
        let mut buf = [0u8; FLOAT_TO_STR_MAX_PAYLOAD];
        let n = format_f64_display(v.to_bits(), &mut buf).expect("fits");
        core::str::from_utf8(&buf[..n]).expect("utf8").to_string()
    }

    #[test]
    fn matches_rust_display_on_boundary_battery() {
        for v in [
            1.0,
            -0.0,
            0.0,
            0.1,
            567.34,
            1e300,
            -1e300,
            5e-324,
            -5e-324,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            f64::EPSILON,
        ] {
            assert_eq!(render(v), format!("{v}"), "Display drift for {v:?}");
        }
    }

    #[test]
    fn integral_drops_fraction_and_negative_zero_keeps_sign() {
        assert_eq!(render(1.0), "1");
        assert_eq!(render(-0.0), "-0");
        assert_eq!(render(0.0), "0");
    }

    #[test]
    fn specials() {
        assert_eq!(render(f64::NAN), "NaN");
        assert_eq!(render(f64::INFINITY), "inf");
        assert_eq!(render(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn worst_case_subnormal_fits_max_payload() {
        let s = render(-5e-324);
        assert_eq!(s.len(), 327, "Display widened past the audited bound");
        assert!(s.len() < FLOAT_TO_STR_MAX_PAYLOAD);
        assert!(
            4 + (FLOAT_TO_STR_MAX_PAYLOAD as u32) <= FLOAT_TO_STR_RECORD_SIZE,
            "record allocation must hold header + max payload"
        );
    }

    #[test]
    fn no_scientific_notation_for_large_magnitudes() {
        let s = render(1e300);
        assert_eq!(s.len(), 301);
        assert!(!s.contains('e') && !s.contains('E'));
    }

    #[test]
    fn too_small_buffer_returns_none() {
        let mut buf = [0u8; 2];
        assert!(format_f64_display(567.34f64.to_bits(), &mut buf).is_none());
    }
}
