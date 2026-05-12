#![forbid(unsafe_code)]

use clap::Parser;
use relon_fmt::{format_source, Error};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "relon-fmt")]
#[command(about = "Format or check Relon files", long_about = None)]
struct Cli {
    /// Check whether files are formatted without writing changes.
    #[arg(long)]
    check: bool,

    /// Print formatted output to stdout instead of writing files.
    #[arg(long)]
    stdout: bool,

    /// Relon files to format.
    files: Vec<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    if cli.files.is_empty() {
        return Err(Error::Usage("expected at least one file".to_string()));
    }

    let mut failed_check = false;
    for file in &cli.files {
        let source = std::fs::read_to_string(file).map_err(|source| Error::Io {
            path: file.clone(),
            source,
        })?;
        let formatted = format_source(&source)?;

        if cli.check {
            if formatted != source {
                eprintln!("{} is not formatted", file.display());
                failed_check = true;
            }
            continue;
        }

        if cli.stdout {
            print!("{formatted}");
        } else if formatted != source {
            std::fs::write(file, formatted).map_err(|source| Error::Io {
                path: file.clone(),
                source,
            })?;
        }
    }

    if failed_check {
        Err(Error::CheckFailed)
    } else {
        Ok(())
    }
}
