//! Trace-abort decisions surfaced by the recorder.
//!
//! Recording stops the first time one of these is raised; the
//! enclosing [`crate::RecorderState`] sticks the value into
//! `aborted: Option<AbortReason>` and every subsequent `record_op`
//! returns [`crate::RecordResult::Abort`] without touching the buffer.
//!
//! Keeping the enum `Copy + Eq` lets callers compare against
//! expected variants in unit tests cheaply.

/// Reason a trace recording was abandoned.
///
/// The variants are designed to be matched exhaustively by the
/// caller so it can decide whether to demote the recording site
/// back to the unspecialised path, blocklist the entry point, or
/// (in the `UnsupportedOp` case) raise a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbortReason {
    /// The recorder saw an `Op` whose [`relon_ir::EffectClass`] is
    /// `UnrecoverableEffect`. No amount of bookkeeping can undo the
    /// effect on a guard failure, so the trace cannot be specialised.
    UnrecoverableEffect,
    /// The recorder does not have a lowering rule for this op yet.
    /// Carries a static name so diagnostics can pinpoint the variant
    /// without re-deriving it. The op family is intentionally a
    /// `&'static str` to keep the abort path allocation-free.
    UnsupportedOp(&'static str),
    /// The recorder hit its op-count budget. The trace is dropped to
    /// avoid blowing the compile-time / code-cache budget on a
    /// runaway path.
    TraceTooLong,
    /// A guard predicate the recorder emitted during recording is
    /// already known to fail — the most common case is observing a
    /// var with one type and then later seeing a different type for
    /// the same var, which would invalidate the type-spec guard.
    GuardFailureInRecording,
}

impl AbortReason {
    /// Stable, human-readable label used by [`std::fmt::Display`] /
    /// log lines. Kept identical to the variant name so grepping
    /// production logs against this enum works.
    pub fn label(self) -> &'static str {
        match self {
            AbortReason::UnrecoverableEffect => "UnrecoverableEffect",
            AbortReason::UnsupportedOp(_) => "UnsupportedOp",
            AbortReason::TraceTooLong => "TraceTooLong",
            AbortReason::GuardFailureInRecording => "GuardFailureInRecording",
        }
    }

    /// True for aborts that are caused by transient conditions
    /// (budget exhausted, hetero type observation) and might
    /// succeed on a future re-record. False for permanent failures
    /// (unrecoverable effect, structurally unsupported op).
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            AbortReason::TraceTooLong | AbortReason::GuardFailureInRecording
        )
    }
}

impl std::fmt::Display for AbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AbortReason::UnsupportedOp(name) => write!(f, "UnsupportedOp({})", name),
            other => f.write_str(other.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_matches_variant_name() {
        assert_eq!(
            AbortReason::UnrecoverableEffect.label(),
            "UnrecoverableEffect"
        );
        assert_eq!(AbortReason::TraceTooLong.label(), "TraceTooLong");
        assert_eq!(
            AbortReason::GuardFailureInRecording.label(),
            "GuardFailureInRecording"
        );
        assert_eq!(AbortReason::UnsupportedOp("Foo").label(), "UnsupportedOp");
    }

    #[test]
    fn unsupported_op_carries_name_in_display() {
        let s = format!("{}", AbortReason::UnsupportedOp("Trap"));
        assert_eq!(s, "UnsupportedOp(Trap)");
    }

    #[test]
    fn transient_classification() {
        assert!(!AbortReason::UnrecoverableEffect.is_transient());
        assert!(!AbortReason::UnsupportedOp("X").is_transient());
        assert!(AbortReason::TraceTooLong.is_transient());
        assert!(AbortReason::GuardFailureInRecording.is_transient());
    }
}
