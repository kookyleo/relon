use crate::tree::AnalyzedTree;
use relon_parser::{NodeId, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;

/// A collection of analyzed files that can resolve references across
/// module boundaries.
#[derive(Default)]
pub struct Workspace {
    pub files: HashMap<String, Arc<AnalyzedTree>>,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, path: String, tree: AnalyzedTree) {
        self.files.insert(path, Arc::new(tree));
    }

    /// Find all references to a specific node (definition) across the
    /// entire workspace.
    pub fn find_references(&self, target_id: NodeId) -> Vec<(String, TokenRange)> {
        let mut results = Vec::new();
        for (path, tree) in &self.files {
            for resolved in tree.references.values() {
                if resolved.target == target_id {
                    results.push((path.clone(), resolved.source_range));
                }
            }
        }
        results
    }

    /// Find all references to a symbol exported from `from_path`.
    /// This follows `#import` chains.
    pub fn find_symbol_references(
        &self,
        _from_path: &str,
        _symbol_name: &str,
    ) -> Vec<(String, TokenRange)> {
        // Placeholder for future implementation.
        // A real implementation would:
        // 1. Find the target NodeId in the source file using symbol_name.
        // 2. Call find_references(target_id).
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    #[test]
    fn find_references_across_files() {
        let mut ws = Workspace::new();

        // File A: defines 'shared_val'
        let src_a = r#"{ shared_val: 42 }"#;
        let node_a = parse_document(src_a).unwrap();
        let tree_a = analyze(&node_a);
        let shared_val_id = if let relon_parser::Expr::Dict(pairs) = &*node_a.expr {
            pairs[0].1.id
        } else {
            panic!()
        };
        ws.add_file("a.relon".to_string(), tree_a);

        // File B: uses 'shared_val'
        // In a real scenario, this would be a cross-file reference resolved by the analyzer.
        // For this test, we manually inject a reference to A's NodeId to verify the indexing.
        let src_b = r#"{ usage: 100 }"#;
        let node_b = parse_document(src_b).unwrap();
        let mut tree_b = analyze(&node_b);

        tree_b.references.insert(
            node_b.id, // placeholder usage site
            crate::resolve::ResolvedRef {
                target: shared_val_id,
                source_range: node_b.range,
                via: relon_parser::RefBase::Sibling,
            },
        );
        ws.add_file("b.relon".to_string(), tree_b);

        let refs = ws.find_references(shared_val_id);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "b.relon");
    }
}
