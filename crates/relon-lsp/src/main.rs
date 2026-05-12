#![forbid(unsafe_code)]

use anyhow::Result;

fn main() -> Result<()> {
    relon_lsp::server::run_stdio()
}
