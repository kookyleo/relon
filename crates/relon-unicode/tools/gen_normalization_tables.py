#!/usr/bin/env python3
"""Generate Unicode normalization tables for the relon-ir crate.

Source: UCD 14.0.0 (UnicodeData.txt, CompositionExclusions.txt,
DerivedNormalizationProps.txt). Produces a single Rust source file
checked in at `crates/relon-unicode/src/normalization_data.rs`, shared by
the tree-walk evaluator (relon-evaluator) and the wasm-AOT backend
(relon-codegen-wasm) so both executors run against the same byte-level
data.

Tables embedded:

  * NFD_INDEX / NFD_POOL:   canonical decomposition mappings, expanded
                            (multi-level decomposition is pre-applied
                            at generation time so the runtime needs only
                            one lookup per code point).
  * NFKD_INDEX / NFKD_POOL: compatibility decomposition mappings;
                            entries without compatibility expansion fall
                            back to the canonical mapping so the lookup
                            is total.
  * CCC_TABLE:              non-zero Canonical_Combining_Class values
                            (the default 0 is implicit — missing entries
                            mean CCC = 0).
  * COMPOSITION_PAIRS:      reverse map for the canonical composition
                            pass. Full_Composition_Exclusion plus the
                            explicit CompositionExclusions.txt list are
                            filtered out at generation time so the
                            runtime never re-checks.

Hangul syllables (U+AC00..=U+D7A3) are decomposed and composed
algorithmically per UAX #15 section 16 — keeping them out of the tables saves
roughly 88 KB.

Run:
    python3 crates/relon-unicode/tools/gen_normalization_tables.py \
        --ucd /path/to/ucd14 \
        --out crates/relon-unicode/src/normalization_data.rs

The generated file is checked in; this script exists only to refresh
it when bumping the Unicode version.
"""

from __future__ import annotations

import argparse
import os
import sys
from typing import Dict, List, Optional, Set, Tuple

HANGUL_S_BASE = 0xAC00
HANGUL_S_LAST = 0xD7A3  # inclusive
HANGUL_L_BASE = 0x1100
HANGUL_V_BASE = 0x1161
HANGUL_T_BASE = 0x11A7


def parse_unicode_data(path: str):
    canonical: Dict[int, List[int]] = {}
    compatibility: Dict[int, List[int]] = {}
    ccc: Dict[int, int] = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            fields = line.split(";")
            cp = int(fields[0], 16)
            ccc_val = int(fields[3])
            if ccc_val != 0:
                ccc[cp] = ccc_val
            decomp = fields[5]
            if not decomp:
                continue
            parts = decomp.split()
            is_compat = parts[0].startswith("<")
            if is_compat:
                parts = parts[1:]
            mapping = [int(p, 16) for p in parts]
            if not is_compat:
                canonical[cp] = mapping
            compatibility[cp] = mapping
    return canonical, compatibility, ccc


def parse_composition_exclusions(path: str) -> Set[int]:
    out: Set[int] = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            hash_idx = line.find("#")
            if hash_idx >= 0:
                line = line[:hash_idx]
            line = line.strip()
            if not line:
                continue
            if ".." in line:
                lo_s, hi_s = line.split("..")
                lo = int(lo_s, 16)
                hi = int(hi_s, 16)
                for cp in range(lo, hi + 1):
                    out.add(cp)
            else:
                out.add(int(line, 16))
    return out


def parse_full_composition_exclusion(path: str) -> Set[int]:
    out: Set[int] = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            hash_idx = line.find("#")
            if hash_idx >= 0:
                line = line[:hash_idx]
            line = line.strip()
            if not line:
                continue
            fields = [p.strip() for p in line.split(";")]
            if len(fields) < 2:
                continue
            prop = fields[1]
            if prop != "Full_Composition_Exclusion":
                continue
            cp_range = fields[0]
            if ".." in cp_range:
                lo_s, hi_s = cp_range.split("..")
                lo = int(lo_s, 16)
                hi = int(hi_s, 16)
                for cp in range(lo, hi + 1):
                    out.add(cp)
            else:
                out.add(int(cp_range, 16))
    return out


def expand_full(
    cp: int,
    table: Dict[int, List[int]],
) -> List[int]:
    """Fully expand a decomposition by recursively replacing each entry."""
    out: List[int] = []
    stack: List[int] = [cp]
    # Use a manual DFS so we never blow the Python stack on degenerate
    # chains. Each pop emits either a terminal cp or pushes children
    # in reverse so the leftmost child is processed first.
    work: List[int] = [cp]
    # simple recursion is fine in CPython for UCD depths.
    def rec(c: int):
        if c in table:
            for ch in table[c]:
                rec(ch)
        else:
            out.append(c)
    rec(cp)
    return out


def build_fully_expanded(table: Dict[int, List[int]]) -> Dict[int, List[int]]:
    """Return a version of `table` where every value is fully expanded."""
    return {cp: expand_full(cp, table) for cp in table}


def flatten(
    table: Dict[int, List[int]],
) -> Tuple[List[Tuple[int, int, int]], List[int]]:
    """Pack mappings into a sorted-by-cp index plus a flat payload pool.

    Returns:
      entries: list of (cp, offset, len) sorted by cp
      payload: list of u32 code points (the actual replacement bytes)
    """
    entries: List[Tuple[int, int, int]] = []
    payload: List[int] = []
    for cp in sorted(table.keys()):
        mapping = table[cp]
        # Skip Hangul syllables — those go through the algorithm.
        if HANGUL_S_BASE <= cp <= HANGUL_S_LAST:
            continue
        entries.append((cp, len(payload), len(mapping)))
        payload.extend(mapping)
    return entries, payload


def emit(
    canonical: Dict[int, List[int]],
    compatibility: Dict[int, List[int]],
    ccc: Dict[int, int],
    composition_pairs: List[Tuple[int, int, int]],
) -> str:
    """Render the generated Rust source."""
    nfd_entries, nfd_pool = flatten(canonical)
    nfkd_entries, nfkd_pool = flatten(compatibility)
    ccc_entries = sorted(ccc.items())
    comp = sorted(composition_pairs, key=lambda t: (t[0], t[1]))

    lines: List[str] = []
    lines.append("// AUTO-GENERATED by crates/relon-unicode/tools/gen_normalization_tables.py")
    lines.append("// from UCD 14.0.0. Do not edit by hand. Re-run the script after a UCD bump.")
    lines.append("//")
    lines.append("// Source files:")
    lines.append("//   UnicodeData.txt          (decomposition mapping + CCC)")
    lines.append("//   DerivedNormalizationProps.txt (Full_Composition_Exclusion)")
    lines.append("//   CompositionExclusions.txt    (explicit exclusion list)")
    lines.append("//")
    lines.append("// Hangul syllables (U+AC00..=U+D7A3) are decomposed and composed")
    lines.append("// algorithmically per UAX #15 section 16 — keeping them out of the tables")
    lines.append("// saves ~88 KB.")
    lines.append("")
    lines.append("/// Sorted by code point. Each entry is")
    lines.append("/// `(cp, payload_offset, payload_len)`. `payload_offset`")
    lines.append("/// indexes into `NFD_POOL`. Hangul syllables are excluded;")
    lines.append("/// callers must run the algorithmic decompose first.")
    lines.append(
        "pub static NFD_INDEX: &[(u32, u32, u8)] = &[",
    )
    for cp, off, ln in nfd_entries:
        lines.append(f"    (0x{cp:04X}, {off}, {ln}),")
    lines.append("];")
    lines.append("")
    lines.append("pub static NFD_POOL: &[u32] = &[")
    for i in range(0, len(nfd_pool), 8):
        chunk = ", ".join(f"0x{c:04X}" for c in nfd_pool[i:i+8])
        lines.append(f"    {chunk},")
    lines.append("];")
    lines.append("")
    lines.append("pub static NFKD_INDEX: &[(u32, u32, u8)] = &[")
    for cp, off, ln in nfkd_entries:
        lines.append(f"    (0x{cp:04X}, {off}, {ln}),")
    lines.append("];")
    lines.append("")
    lines.append("pub static NFKD_POOL: &[u32] = &[")
    for i in range(0, len(nfkd_pool), 8):
        chunk = ", ".join(f"0x{c:04X}" for c in nfkd_pool[i:i+8])
        lines.append(f"    {chunk},")
    lines.append("];")
    lines.append("")
    lines.append(
        "/// Canonical_Combining_Class, sparse (only non-zero entries).",
    )
    lines.append("/// Sorted by code point. Lookup falls back to 0 when absent.")
    lines.append("pub static CCC_TABLE: &[(u32, u8)] = &[")
    for cp, val in ccc_entries:
        lines.append(f"    (0x{cp:04X}, {val}),")
    lines.append("];")
    lines.append("")
    lines.append("/// Canonical composition pairs, sorted by")
    lines.append("/// `(first, second)`. Excludes any pair whose composite")
    lines.append("/// has Full_Composition_Exclusion = True or appears in")
    lines.append("/// CompositionExclusions.txt. Hangul composition runs")
    lines.append("/// through its own algorithmic helper.")
    lines.append(
        "pub static COMPOSITION_PAIRS: &[(u32, u32, u32)] = &[",
    )
    for first, second, composed in comp:
        lines.append(f"    (0x{first:04X}, 0x{second:04X}, 0x{composed:04X}),")
    lines.append("];")
    lines.append("")
    return "\n".join(lines)


def main(argv: List[str]) -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--ucd", required=True, help="dir containing UCD files")
    p.add_argument("--out", required=True, help="output Rust file path")
    args = p.parse_args(argv)

    canonical, compatibility, ccc = parse_unicode_data(
        os.path.join(args.ucd, "UnicodeData.txt"),
    )
    explicit_excl = parse_composition_exclusions(
        os.path.join(args.ucd, "CompositionExclusions.txt"),
    )
    full_excl = parse_full_composition_exclusion(
        os.path.join(args.ucd, "DerivedNormalizationProps.txt"),
    )
    exclusions = explicit_excl | full_excl

    # Build canonical composition pairs from canonical decompositions of
    # length 2 whose composite is not on the exclusion list.
    composition_pairs: List[Tuple[int, int, int]] = []
    for cp, mapping in canonical.items():
        if len(mapping) != 2:
            continue
        if cp in exclusions:
            continue
        composition_pairs.append((mapping[0], mapping[1], cp))

    # Build fully-expanded canonical / compatibility tables so the
    # lookup-then-recurse loop in Rust stops after one substitution.
    canonical_full = build_fully_expanded(canonical)
    compatibility_full = build_fully_expanded(compatibility)

    src = emit(canonical_full, compatibility_full, ccc, composition_pairs)
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(src)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
