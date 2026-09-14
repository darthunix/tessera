//! Command-line selection of full or diagnostic runs and saved comparisons.
//!
//! Keep quick/filtered runs from being saved as baselines, allow raw results
//! in either mode, and validate names before timing. Selection changes
//! which cases run, never the timing method or the performance limits.

use super::baseline::{self, Kind};
use anyhow::{Context, Result, bail, ensure};
use std::path::PathBuf;

#[derive(Default, Debug)]
pub struct Options {
    pub quick: bool,
    pub previous: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub save_results: Option<PathBuf>,
    pub filter: Option<String>,
}

impl Options {
    pub fn help(definition: baseline::Definition, full: usize, quick: usize) -> String {
        format!(
            "{} [--quick] [--filter TEXT] [--baseline NAME]
    [--save-baseline NAME] [--save-results NAME]
\n{full} full cases; --quick selects {quick} diagnostic cases.
Three complete series of 15 paired samples; {} limit; control strictly +/-3%.
FAIL and UNSTABLE exit nonzero. No retries or discarded measurements.
--save-results saves complete selected cases, including diagnostics and failures.
--save-baseline requires a full PASS, including comparison with --baseline.
Both save flags may be combined. Existing files are never overwritten.
Baselines live in target/{}; results in target/{}.
Results cannot be used as baselines. CPU, OS, compiler, flags, benchmark sources,
manifests, lockfile and the full case set must match. Incompatible files are
rejected, not migrated. See benches/README.md for methodology.",
            definition.name,
            definition.policy,
            definition.directory(Kind::Baseline),
            definition.directory(Kind::Results)
        )
    }

    pub fn parse(
        args: impl IntoIterator<Item = String>,
        definition: baseline::Definition,
    ) -> Result<Option<Self>> {
        let mut result = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--bench" => {}
                "--quick" => result.quick = true,
                "--help" | "-h" => return Ok(None),
                "--baseline" | "--save-baseline" | "--save-results" => {
                    let (destination, kind) = match arg.as_str() {
                        "--baseline" => (&mut result.previous, Kind::Baseline),
                        "--save-baseline" => (&mut result.save, Kind::Baseline),
                        _ => (&mut result.save_results, Kind::Results),
                    };
                    *destination = Some(baseline::path_for(
                        &args.next().context("missing saved-run name")?,
                        definition,
                        kind,
                    )?);
                }
                "--filter" => result.filter = Some(args.next().context("missing filter")?),
                _ => bail!("unknown argument: {arg}"),
            }
        }
        ensure!(
            result.save.is_none() || !result.is_diagnostic(),
            "cannot save a quick or filtered baseline"
        );
        for path in [&result.save, &result.save_results].into_iter().flatten() {
            ensure!(
                !path.exists(),
                "saved run already exists: {}",
                path.display()
            );
        }
        Ok(Some(result))
    }

    pub fn is_diagnostic(&self) -> bool {
        self.quick || self.filter.is_some()
    }

    pub fn matches(&self, name: &str, quick_case: bool) -> bool {
        (!self.quick || quick_case)
            && self
                .filter
                .as_ref()
                .is_none_or(|filter| name.contains(filter))
    }
}
