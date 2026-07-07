//! Staleness insurance for the object cache's generator stamp.
//!
//! `GENERATOR_VERSION` (defined in
//! `relon-codegen-cranelift/src/object_cache_integration.rs`) is
//! mixed into every cache key and metadata trailer so that stale
//! objects self-invalidate when the code generator changes. The
//! stamp is bumped by hand — and the highest-risk missed bump is a
//! change to the relon-IR `Op` enum: any added / removed / reordered
//! variant changes what compiled objects mean, and a forgotten bump
//! lets old cache files keep hitting.
//!
//! This test turns that convention into a build gate, deliberately
//! without any build-script magic: it reads the two source files off
//! disk, extracts the `Op` enum definition block and the
//! `GENERATOR_VERSION` string, and asserts both against the pins
//! below. Changing the `Op` enum therefore goes red here until the
//! author bumps `GENERATOR_VERSION` *and* re-pins the pair in the
//! same commit — the diff of this file is the audit record that the
//! bump was considered.
//!
//! The hash is over the raw text of the enum block, so comment-only
//! edits inside the enum also trip it. That is accepted noise: in
//! that case re-pin `PINNED_OP_ENUM_SHA256` without bumping
//! `GENERATOR_VERSION`, and say so in the commit message.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Must equal the `GENERATOR_VERSION` constant in
/// `relon-codegen-cranelift/src/object_cache_integration.rs`.
const PINNED_GENERATOR_VERSION: &str = "relon-codegen-cranelift v5-gamma 18";

/// SHA-256 (hex) of the `pub enum Op { ... }` block in
/// `relon-ir/src/ir.rs`, from the `pub enum Op {` line through the
/// first column-zero `}` line, inclusive, with `\n` line endings.
const PINNED_OP_ENUM_SHA256: &str =
    "0ea9fa40e1a3ce129e0ae77f3a2463ac09ccfb969b25223a9fd92392de875c09";

fn crates_root() -> PathBuf {
    // crates/relon-object-cache -> crates/
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate lives under crates/")
        .to_path_buf()
}

/// Extract the `pub enum Op { ... }` block: the declaration line
/// through the first line that is exactly `}` (the enum's closing
/// brace sits at column zero; every variant and nested brace is
/// indented).
fn op_enum_block(ir_source: &str) -> String {
    let mut block = String::new();
    let mut in_block = false;
    for line in ir_source.lines() {
        if !in_block {
            if line.starts_with("pub enum Op {") {
                in_block = true;
            } else {
                continue;
            }
        }
        block.push_str(line);
        block.push('\n');
        if in_block && line == "}" {
            return block;
        }
    }
    panic!("could not extract `pub enum Op {{ ... }}` block from relon-ir/src/ir.rs");
}

/// Extract the string literal assigned to `pub const
/// GENERATOR_VERSION` — plain text matching, no parsing.
fn generator_version(integration_source: &str) -> String {
    let marker = "pub const GENERATOR_VERSION: &str = \"";
    let start = integration_source
        .find(marker)
        .expect("GENERATOR_VERSION const not found in object_cache_integration.rs")
        + marker.len();
    let rest = &integration_source[start..];
    let end = rest
        .find('"')
        .expect("unterminated GENERATOR_VERSION literal");
    rest[..end].to_string()
}

#[test]
fn generator_version_is_bound_to_the_op_enum_shape() {
    let root = crates_root();
    let ir_path = root.join("relon-ir/src/ir.rs");
    let integration_path = root.join("relon-codegen-cranelift/src/object_cache_integration.rs");
    if !ir_path.is_file() || !integration_path.is_file() {
        // Outside the workspace checkout (e.g. a packaged crate) the
        // sibling sources are absent and the guard has nothing to
        // bind; it only has teeth where the enum can actually change.
        eprintln!("skipping: sibling crate sources not present (not a workspace checkout)");
        return;
    }

    let ir_source = std::fs::read_to_string(&ir_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", ir_path.display()));
    let integration_source = std::fs::read_to_string(&integration_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", integration_path.display()));

    let actual_version = generator_version(&integration_source);
    let actual_hash = hex::encode(Sha256::digest(op_enum_block(&ir_source).as_bytes()));

    assert_eq!(
        actual_hash, PINNED_OP_ENUM_SHA256,
        "\nthe relon-IR `Op` enum definition changed but the cache generator stamp \
         was not re-audited.\n\
         Cached objects compiled against the old enum may silently mean something \
         else now.\n\
         Fix: bump GENERATOR_VERSION in \
         relon-codegen-cranelift/src/object_cache_integration.rs (with a rationale \
         doc-comment line, as every previous bump has), then update BOTH \
         PINNED_GENERATOR_VERSION and PINNED_OP_ENUM_SHA256 in this test.\n\
         (If the enum edit was comment-only, re-pin the hash without bumping and \
         say so in the commit message.)\n"
    );
    assert_eq!(
        actual_version, PINNED_GENERATOR_VERSION,
        "\nGENERATOR_VERSION moved; update PINNED_GENERATOR_VERSION (and, if the \
         `Op` enum also changed, PINNED_OP_ENUM_SHA256) in this test so the pinned \
         pair stays the audit record binding the stamp to the enum shape.\n"
    );
}
