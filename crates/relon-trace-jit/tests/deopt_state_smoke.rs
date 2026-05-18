//! DeoptState construction / apply / roundtrip smoke tests.

use std::collections::HashMap;

use relon_trace_jit::{
    DeoptState, ExternalAddr, ExternalPc, ExternalSlot, GuardKind, GuardSite, ObservedType,
    RecoverableWrite, SsaVar,
};

#[test]
fn empty_state_is_empty() {
    let s = DeoptState::new();
    assert!(s.is_empty());
}

#[test]
fn apply_restores_slots_in_order() {
    let mut s = DeoptState::new();
    s.bind(SsaVar(0), ExternalSlot(100));
    s.bind(SsaVar(1), ExternalSlot(200));
    s.bind(SsaVar(2), ExternalSlot(300));
    let mut restored: Vec<(SsaVar, ExternalSlot)> = Vec::new();
    s.apply(|v, slot| restored.push((v, slot)), |_, _| {});
    assert_eq!(restored.len(), 3);
    assert_eq!(restored[0], (SsaVar(0), ExternalSlot(100)));
    assert_eq!(restored[2], (SsaVar(2), ExternalSlot(300)));
}

#[test]
fn apply_replays_recoverable_writes_in_order() {
    let mut s = DeoptState::new();
    s.record_recoverable_write(RecoverableWrite {
        addr: ExternalAddr(1),
        before_value: 11,
    });
    s.record_recoverable_write(RecoverableWrite {
        addr: ExternalAddr(2),
        before_value: 22,
    });
    let mut replayed: Vec<(ExternalAddr, u64)> = Vec::new();
    s.apply(|_, _| {}, |a, v| replayed.push((a, v)));
    assert_eq!(replayed, vec![(ExternalAddr(1), 11), (ExternalAddr(2), 22)]);
}

#[test]
fn full_deopt_simulation_restores_state() {
    // Pretend we have generic-code memory (HashMap-modelled) that the
    // trace mutated. After a guard fires we apply the deopt state
    // and verify memory matches the pre-trace snapshot.
    let mut s = DeoptState::new();
    s.bind(SsaVar(0), ExternalSlot(7));
    s.record_recoverable_write(RecoverableWrite {
        addr: ExternalAddr(0xCAFE),
        before_value: 1234,
    });

    let mut memory: HashMap<ExternalAddr, u64> = HashMap::new();
    let mut slots: HashMap<SsaVar, ExternalSlot> = HashMap::new();
    // Trace had moved cursor forward to 9999.
    memory.insert(ExternalAddr(0xCAFE), 9999);

    s.apply(
        |ssa, slot| {
            slots.insert(ssa, slot);
        },
        |addr, val| {
            memory.insert(addr, val);
        },
    );
    assert_eq!(memory[&ExternalAddr(0xCAFE)], 1234);
    assert_eq!(slots[&SsaVar(0)], ExternalSlot(7));
}

#[test]
fn guardsite_starts_empty() {
    let gs = GuardSite::new(
        4,
        ExternalPc(0x88),
        GuardKind::BoundsCheck(SsaVar(1), SsaVar(2)),
    );
    assert!(gs.deopt_state.is_empty());
    assert_eq!(gs.trace_pc, 4);
}

#[test]
fn bincode_roundtrip_deopt_state() {
    let mut s = DeoptState::new();
    s.bind(SsaVar(0), ExternalSlot(100));
    s.bind(SsaVar(1), ExternalSlot(101));
    s.record_recoverable_write(RecoverableWrite {
        addr: ExternalAddr(42),
        before_value: 9000,
    });
    let bytes = bincode::serialize(&s).expect("encode");
    let back: DeoptState = bincode::deserialize(&bytes).expect("decode");
    assert_eq!(back, s);
}

#[test]
fn bind_overwrites_existing_slot() {
    let mut s = DeoptState::new();
    s.bind(SsaVar(0), ExternalSlot(100));
    s.bind(SsaVar(0), ExternalSlot(101));
    s.bind(SsaVar(0), ExternalSlot(102));
    assert_eq!(s.ssa_to_external_slot.len(), 1);
    assert_eq!(s.ssa_to_external_slot[0], (SsaVar(0), ExternalSlot(102)));
}

#[test]
fn guard_kind_serialization_stable() {
    let gk = GuardKind::TypeCheck(SsaVar(3), ObservedType::I64);
    let s = bincode::serialize(&gk).unwrap();
    let back: GuardKind = bincode::deserialize(&s).unwrap();
    assert_eq!(gk, back);
}
