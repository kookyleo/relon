//! Semantic analysis layer for Relon.
//!
//! Sits between `relon-parser` (raw AST) and `relon-evaluator` (runtime
//! tree-walk). Responsibilities that this crate gradually absorbs from the
//! evaluator:
//!
//! * Schema desugar — `@schema` annotated dicts lowered to a `SchemaDef` IR.
//! * Diagnostic aggregation — collect every structural issue in one pass
//!   instead of fail-fast.
//! * (Future) Name resolution — bind `Reference { base, path }` to a
//!   stable target id.
//! * (Future) Module graph — pre-resolve `@import` paths.
//!
//! The output is an [`AnalyzedTree`] keyed by [`relon_parser::NodeId`], so
//! the AST itself stays immutable and consumers (evaluator, LSP, lint)
//! pick up just the side-tables they need.

pub(crate) mod decorator_names;
pub mod diagnostic;
pub mod modules;
pub mod resolve;
pub mod schema;
pub mod tree;
pub mod typecheck;
pub mod workspace;

pub use diagnostic::{Diagnostic, Severity};
pub use modules::ModuleImport;
pub use resolve::ResolvedRef;
pub use schema::{
    lower_schema_pure, BaseRef, EnumVariant, MetaDecoratorRef, SchemaDef, SchemaFieldDef,
};
pub use tree::AnalyzedTree;
pub use workspace::Workspace;

use relon_parser::Node;

/// Run every analyzer pass over `root` and return the aggregated tree.
///
/// Errors are collected into [`AnalyzedTree::diagnostics`] rather than
/// short-circuiting. Use [`AnalyzedTree::has_errors`] to decide whether
/// to continue to evaluation.
pub fn analyze(root: &Node) -> AnalyzedTree {
    let mut tree = AnalyzedTree::new();
    schema::collect_schemas(root, &mut tree);
    resolve::resolve_references(root, &mut tree);
    modules::collect_imports(root, &mut tree);
    typecheck::typecheck(root, &mut tree);
    tree
}
