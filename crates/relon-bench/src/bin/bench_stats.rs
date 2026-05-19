#![forbid(unsafe_code)]

//! CLI helper that prints a Markdown distribution table for a criterion
//! benchmark group.
//!
//! Default usage:
//!
//! ```text
//! cargo run -p relon-bench --bin bench_stats -- \
//!     target/criterion/v6_epsilon_hot_loop
//! ```
//!
//! Walks `<group_root>/<dim>/<row>/new/sample.json`, extracts each
//! sample's `times[i] / iters[i]` ns/iter estimate, sorts, and prints
//! p50/p90/p99/p99.9/max per row. See `bench_stats` module docs for
//! the methodology rationale.

use std::path::PathBuf;
use std::process::ExitCode;

use relon_bench::bench_stats::{collect_group_stats, render_markdown_table};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let group_root = match args.get(1) {
        Some(s) => PathBuf::from(s),
        None => {
            eprintln!("usage: bench_stats <group_root>");
            eprintln!();
            eprintln!("  <group_root>  e.g. target/criterion/v6_epsilon_hot_loop");
            return ExitCode::from(2);
        }
    };
    match collect_group_stats(&group_root) {
        Ok(rows) if rows.is_empty() => {
            eprintln!("bench_stats: no rows found under {}", group_root.display());
            ExitCode::from(3)
        }
        Ok(rows) => {
            println!("# Distribution table for {}", group_root.display());
            println!();
            println!("{}", render_markdown_table(&rows));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bench_stats: {e}");
            ExitCode::FAILURE
        }
    }
}
