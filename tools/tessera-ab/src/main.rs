//! Two revisions of Tessera compared for a change that should not move
//! anything: `measure` times bench/pg families with each revision
//! installed in turn, `moved` checks that a cut only moved lines between
//! files. See README.md.
#![forbid(unsafe_code)]

mod measure;
mod moved;
mod stats;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    about = "Compare two revisions of Tessera at the level of PostgreSQL.",
    after_help = "Exit of measure: 0 nothing slower, 1 a case slower past the threshold, 2 an error.\nExit of moved: 0 every line of the base moved, 1 a line missing, 2 an error."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Time bench/pg families with the base and the candidate installed in
    /// turn, and compare them case by case against the control without
    /// Tessera. Installs into the build PG_CONFIG names.
    Measure(measure::Options),
    /// Check that a cut only moved lines: every non-blank line of the
    /// base's --from files is in one of the candidate's --to files, as many
    /// times; a line that lost its `static` is listed apart.
    Moved(moved::Options),
}

fn main() -> ExitCode {
    let result = measure::repository().and_then(|repo| match Cli::parse().command {
        Commands::Measure(options) => measure::run(&repo, &options),
        Commands::Moved(options) => moved::run(&repo, &options),
    });
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ERROR: {error:#}");
            ExitCode::from(2)
        }
    }
}
