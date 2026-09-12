//! Command-line selection of full or diagnostic runs and saved comparisons.
//!
//! Keep quick/filtered runs from being saved as complete baselines, validate
//! baseline names, and provide the benchmark's help text. Selection changes
//! which cases run, never the timing method or the performance limits.

use super::baseline;
use anyhow::{Context, Result, bail, ensure};
use std::path::PathBuf;

pub const HELP: &str =
    "column_reader [--quick] [--baseline NAME] [--save-baseline NAME] [--filter TEXT]

Three complete series of 15 paired samples; 3% limit. A full run takes several minutes.
Without selection options, all 36 cases run. --quick selects 14 diagnostic cases
with the same sampling and limits; it is not a complete performance check.
Each sample brackets one reader with reference runs. A self-reference control
must stay within +/-3%. Reports PASS, FAIL, or UNSTABLE for each reading path.
Both FAIL and UNSTABLE return a nonzero exit status.
--baseline NAME also compares against a compatible full v2 saved run (3% limit).
It can be combined with --quick and/or --filter to compare just selected cases.
--save-baseline NAME saves raw samples only for a complete PASS; never overwrites.
--filter TEXT selects matching names, within the quick subset when --quick is set.
Neither --quick nor --filter can be combined with --save-baseline.
Baselines live in target/column-reader-baselines. Matching CPU, OS, compiler,
Rust flags, benchmark sources, manifests, lockfile, and case set are required.
v1 baselines are not compatible.";

#[derive(Default, Debug)]
pub struct Options {
    pub quick: bool,
    pub previous: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub filter: Option<String>,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>> {
        let mut result = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--bench" => {}
                "--quick" => result.quick = true,
                "--help" | "-h" => return Ok(None),
                "--baseline" => {
                    result.previous = Some(baseline::path_for(
                        &args.next().context("missing baseline name")?,
                    )?)
                }
                "--save-baseline" => {
                    result.save = Some(baseline::path_for(
                        &args.next().context("missing baseline name")?,
                    )?)
                }
                "--filter" => result.filter = Some(args.next().context("missing filter")?),
                _ => bail!("unknown argument: {arg}"),
            }
        }
        ensure!(
            result.save.is_none() || !result.is_diagnostic(),
            "cannot save a quick or filtered baseline"
        );
        if let Some(path) = &result.save {
            ensure!(
                !path.exists(),
                "baseline already exists: {}",
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
