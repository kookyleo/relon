//! LSP features that consume `relon-analyzer`'s side-tables.
//!
//! Every feature follows the same shape: take a `DocumentEntry`
//! (cached source + AST + analyzed tree) and a cursor, return an
//! LSP-shaped response. They don't touch I/O — `server.rs` owns
//! the dispatch and replies.

pub mod completion;
pub mod cursor;
pub mod definition;
pub mod hover;
