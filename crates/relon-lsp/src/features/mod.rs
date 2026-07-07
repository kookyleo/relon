//! LSP features that consume `relon-analyzer`'s side-tables.
//!
//! Every feature follows the same shape: take a `DocumentEntry`
//! (cached source + AST + analyzed tree) and a cursor, return an
//! LSP-shaped response. They don't touch I/O — `server.rs` owns
//! the dispatch and replies. ([`formatting`] is the one exception:
//! it consumes the raw source only, since `relon-fmt` runs its own
//! parse.)

pub mod completion;
pub mod cursor;
pub mod definition;
pub mod formatting;
pub mod hover;
pub mod references;
