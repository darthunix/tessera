//! Two revisions of Tessera compared for a change that should not move
//! anything: `measure` times bench/pg families with each revision
//! installed in turn. See README.md.
#![forbid(unsafe_code)]

mod measure;
mod stats;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    about = "Compare two revisions of Tessera at the level of PostgreSQL.",
    after_help = "Exit: 0 nothing slower, 1 a case slower past the threshold, 2 an error."
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
}

fn main() -> ExitCode {
    let result = measure::repository().and_then(|repo| match Cli::parse().command {
        Commands::Measure(options) => measure::run(&repo, &options),
    });
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ERROR: {error:#}");
            ExitCode::from(2)
        }
    }
}
