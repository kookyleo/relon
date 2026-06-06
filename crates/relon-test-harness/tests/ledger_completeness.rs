//! Bijection guard between the lowering cap registry and the coverage
//! ledger.
//!
//! The lowering pass wraps every `LoweringError` construction in `cap!`
//! with a stable id registered in
//! `relon_ir::lowering::cap::LOWERING_CAP_IDS`. The coverage ledger
//! (`relon_test_harness::ledger::LEDGER`) carries one human-auditable row
//! per id. These two tests assert the two sets are in exact bijection:
//!
//! * no cap id is missing from the ledger (a new, unaudited cap fails
//!   the build), and
//! * no ledger row references an id that is not a registered cap
//!   (a stale row after a cap is retired fails the build).
//!
//! Together with the macro's in-registry `debug_assert!`, this is the
//! "no silent new cap" guard for the full-coverage effort.

use std::collections::HashSet;

use relon_ir::lowering::cap::LOWERING_CAP_IDS;
use relon_test_harness::ledger::LEDGER;

#[test]
fn cap_ids_are_unique() {
    let mut seen = HashSet::new();
    for id in LOWERING_CAP_IDS {
        assert!(seen.insert(*id), "duplicate id in LOWERING_CAP_IDS: {id}");
    }
}

#[test]
fn ledger_ids_are_unique() {
    let mut seen = HashSet::new();
    for entry in LEDGER {
        assert!(
            seen.insert(entry.id),
            "duplicate id in LEDGER: {}",
            entry.id
        );
    }
}

#[test]
fn every_cap_id_has_a_ledger_entry() {
    let ledger: HashSet<&str> = LEDGER.iter().map(|e| e.id).collect();
    let missing: Vec<&str> = LOWERING_CAP_IDS
        .iter()
        .copied()
        .filter(|id| !ledger.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "cap ids with no LEDGER entry (add a row): {missing:?}"
    );
}

#[test]
fn no_orphan_ledger_entries() {
    let registry: HashSet<&str> = LOWERING_CAP_IDS.iter().copied().collect();
    let orphans: Vec<&str> = LEDGER
        .iter()
        .map(|e| e.id)
        .filter(|id| !registry.contains(id))
        .collect();
    assert!(
        orphans.is_empty(),
        "LEDGER rows referencing unregistered cap ids (cap retired?): {orphans:?}"
    );
}

#[test]
fn ledger_and_registry_are_same_size() {
    // Redundant with the bijection tests above when both id sets are
    // unique, but pins the count so an accidental dup-vs-drop that
    // happens to net out is still caught.
    assert_eq!(
        LEDGER.len(),
        LOWERING_CAP_IDS.len(),
        "LEDGER ({}) and LOWERING_CAP_IDS ({}) differ in length",
        LEDGER.len(),
        LOWERING_CAP_IDS.len()
    );
}
