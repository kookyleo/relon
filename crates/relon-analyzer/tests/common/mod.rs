//! Shared helpers for the analyzer's feature-themed integration tests.
//!
//! Each sibling `*_fixtures.rs`-style test file (`main_signature.rs`,
//! `strict_mode.rs`, …) parses one or more `.relon` fixtures off disk
//! and asserts on the resulting diagnostic shape. The helpers below
//! centralize fixture loading, single-file parse + analyze, the
//! `count` predicate counter, and the disk-backed `ModuleLoader`
//! used by multi-file workspace tests. Cargo treats `tests/common/`
//! as a non-binary module by virtue of the `mod.rs` filename, so each
//! test crate pulls these in via `mod common; use common::*;`.

#![allow(dead_code)]

use relon_analyzer::{
    analyze, analyze_entry,
    workspace::{LoadError, LoadedModule, ModuleLoader},
    AnalyzedTree, Diagnostic, WorkspaceTree,
};
use relon_parser::parse_document;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn load_fixture(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {rel}: {e}"))
}

pub fn analyze_fixture(rel: &str) -> Arc<AnalyzedTree> {
    let src = load_fixture(rel);
    let node = parse_document(&src).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
    Arc::new(analyze(&node))
}

pub fn count<F: Fn(&Diagnostic) -> bool>(diags: &[Diagnostic], pred: F) -> usize {
    diags.iter().filter(|d| pred(d)).count()
}

/// Disk-backed loader scoped at a fixture subdirectory. Maps relative
/// `#import "./X.relon"` paths to the file contents.
pub struct DiskLoader {
    root: PathBuf,
    canonical: HashMap<String, String>,
}

impl DiskLoader {
    pub fn new(rel_dir: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(rel_dir);
        Self {
            root,
            canonical: HashMap::new(),
        }
    }
}

impl ModuleLoader for DiskLoader {
    fn load(&mut self, path: &str, _current_dir: &Path) -> Result<LoadedModule, LoadError> {
        let p = self.root.join(path.trim_start_matches("./"));
        let canonical_id = p.to_string_lossy().to_string();
        let source = std::fs::read_to_string(&p).map_err(|_| LoadError::NotFound)?;
        self.canonical
            .insert(path.to_string(), canonical_id.clone());
        Ok(LoadedModule {
            canonical_id,
            source,
            current_dir: self.root.clone(),
        })
    }
}

/// Multi-file workspace pass over a fixture subdirectory. `entry_rel`
/// is relative to `tests/fixtures/<sub_dir>/`.
pub fn analyze_fixture_workspace(sub_dir: &str, entry_rel: &str) -> WorkspaceTree {
    let mut loader = DiskLoader::new(sub_dir);
    let entry_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(sub_dir)
        .join(entry_rel);
    let src = std::fs::read_to_string(&entry_path).unwrap();
    analyze_entry(
        entry_path.to_string_lossy().to_string(),
        &src,
        entry_path.parent().unwrap().to_path_buf(),
        &mut loader,
    )
}
