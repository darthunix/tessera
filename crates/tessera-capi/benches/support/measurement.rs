//! Raw timing samples and performance assessment, separate from the clock loop.
//!
//! Compare each reader with its surrounding reference runs and, optionally,
//! saved measurements. Three series distinguish consistent overhead from
//! unstable timings; a reference-versus-itself control detects measurement noise.
//! Keep every sample. Full and diagnostic runs use the same policy: strict 3%
//! for readers, both 3% and 1 ns for filters, and strict +/-3% for controls.

use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, fmt};

pub const SERIES: usize = 3;
pub const SAMPLES: usize = 15;
pub const LIMIT: f64 = 1.03;
const CONTROL_MIN: f64 = 0.97;

/// Reader checks stay strictly relative; filtering also allows 1 ns per call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Strict,
    Filter,
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Strict => "3%",
            Self::Filter => ">3% AND >1ns",
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Comparison {
    pub ratios: Ratios,
    /// Median paired difference, normalized to the current reference clock.
    pub delta_ns: f64,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub before_ns: f64,
    pub measured_ns: f64,
    pub after_ns: f64,
}

impl Sample {
    pub fn reference_ns(self) -> f64 {
        self.before_ns / 2. + self.after_ns / 2.
    }

    pub fn ratio(self) -> f64 {
        self.measured_ns / self.reference_ns()
    }

    pub fn validate(self) -> Result<()> {
        ensure!(
            [
                self.before_ns,
                self.measured_ns,
                self.after_ns,
                self.ratio()
            ]
            .into_iter()
            .all(|value| value.is_finite() && value > 0.),
            "measurements and ratios must be finite and positive"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Unstable,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Unstable => "UNSTABLE",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ratios {
    // Median of all 45 paired ratios; bounds are the three series medians.
    // These bounds describe repeatability, not a confidence interval.
    pub median: f64,
    pub low: f64,
    pub high: f64,
}

impl Ratios {
    pub fn status(self) -> Status {
        if self.high <= LIMIT {
            Status::Pass
        } else if self.low > LIMIT {
            Status::Fail
        } else {
            Status::Unstable
        }
    }

    pub fn control_ok(self) -> bool {
        self.low >= CONTROL_MIN && self.high <= LIMIT
    }

    pub fn relative_to(self, previous: Self) -> Self {
        Self {
            median: self.median / previous.median,
            low: self.low / previous.high,
            high: self.high / previous.low,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Summary {
    pub reference_ns: f64,
    pub measured_ns: f64,
    pub ratios: Ratios,
}

#[derive(Debug)]
pub struct Assessment {
    pub reference: Status,
    pub previous: Option<Status>,
    pub status: Status,
    pub reason: &'static str,
}

impl Assessment {
    pub fn new(reference: Status, previous: Option<Status>, control_ok: bool) -> Self {
        let reference = if control_ok {
            reference
        } else {
            Status::Unstable
        };
        let previous = previous.map(|status| if control_ok { status } else { Status::Unstable });
        let (status, reason) = if !control_ok {
            (Status::Unstable, "self-reference control outside +/-3%")
        } else if reference == Status::Fail && previous == Some(Status::Fail) {
            (
                Status::Fail,
                "above reference limit and slower than baseline",
            )
        } else if reference == Status::Fail {
            (Status::Fail, "above reference limit")
        } else if previous == Some(Status::Fail) {
            (Status::Fail, "slower than baseline")
        } else if reference == Status::Unstable || previous == Some(Status::Unstable) {
            (Status::Unstable, "series range crosses a performance limit")
        } else {
            (Status::Pass, "all comparison ranges within limits")
        };
        Self {
            reference,
            previous,
            status,
            reason,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Measurement {
    samples: BTreeMap<&'static str, [[Option<Sample>; SAMPLES]; SERIES]>,
}

impl Measurement {
    /// Compare paired samples, retaining both runs' series ranges for history.
    ///
    /// For history, an old ratio r predicts current time reference_ns * r.
    /// Lower/upper margins use the old high/low ratio respectively, matching
    /// the conservative bounds of Ratios::relative_to. Equality passes.
    pub fn comparison(
        &self,
        path: &'static str,
        previous: Option<&Self>,
        policy: Policy,
    ) -> Result<Comparison> {
        let current = self.summary(path)?;
        let old = previous.map(|old| old.summary(path)).transpose()?;
        let previous = old.map_or(
            Ratios {
                median: 1.,
                low: 1.,
                high: 1.,
            },
            |old| old.ratios,
        );
        let ratios = current.ratios.relative_to(previous);
        let mut deltas = [0.; SERIES * SAMPLES];
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for series in 0..SERIES {
            let mut lower = [0.; SAMPLES];
            let mut upper = [0.; SAMPLES];
            for index in 0..SAMPLES {
                let sample = self.sample(path, series, index)?;
                let reference = sample.reference_ns();
                deltas[series * SAMPLES + index] = sample.measured_ns - reference * previous.median;
                let margin = |old_ratio: f64| {
                    let expected = reference * old_ratio;
                    let absolute = if policy == Policy::Filter { 1. } else { 0. };
                    sample.measured_ns - (expected * LIMIT).max(expected + absolute)
                };
                lower[index] = margin(previous.high);
                upper[index] = margin(previous.low);
            }
            low = low.min(median(&mut lower));
            high = high.max(median(&mut upper));
        }
        let status = if policy == Policy::Strict {
            // Preserve the existing reader gate, including boundary rounding.
            ratios.status()
        } else if high <= 0. {
            Status::Pass
        } else if low > 0. {
            Status::Fail
        } else {
            Status::Unstable
        };
        Ok(Comparison {
            ratios,
            delta_ns: median(&mut deltas),
            status,
        })
    }

    pub fn insert(
        &mut self,
        path: &'static str,
        series: usize,
        index: usize,
        sample: Sample,
    ) -> Result<()> {
        sample.validate()?;
        let slot = self
            .samples
            .entry(path)
            .or_insert([[None; SAMPLES]; SERIES])
            .get_mut(series)
            .and_then(|series| series.get_mut(index))
            .context("sample index out of bounds")?;
        ensure!(slot.is_none(), "duplicate measurement sample");
        *slot = Some(sample);
        Ok(())
    }

    pub fn sample(&self, path: &'static str, series: usize, index: usize) -> Result<Sample> {
        self.samples
            .get(path)
            .context("missing measurement path")?
            .get(series)
            .and_then(|series| series.get(index))
            .copied()
            .flatten()
            .context("missing measurement sample")
    }

    pub fn summary(&self, path: &'static str) -> Result<Summary> {
        let mut ratios = [0.; SERIES * SAMPLES];
        let mut references = ratios;
        let mut measured = ratios;
        let mut medians = [0.; SERIES];
        for (series, series_median) in medians.iter_mut().enumerate() {
            for index in 0..SAMPLES {
                let sample = self.sample(path, series, index)?;
                let flat_index = series * SAMPLES + index;
                ratios[flat_index] = sample.ratio();
                references[flat_index] = sample.reference_ns();
                measured[flat_index] = sample.measured_ns;
            }
            *series_median = median(&mut ratios[series * SAMPLES..(series + 1) * SAMPLES]);
        }
        medians.sort_by(f64::total_cmp);
        Ok(Summary {
            reference_ns: median(&mut references),
            measured_ns: median(&mut measured),
            ratios: Ratios {
                median: median(&mut ratios),
                low: medians[0],
                high: medians[SERIES - 1],
            },
        })
    }

    /// A complete selected case is valid data even when its timings fail a gate.
    pub fn validate(&self, paths: &[&'static str]) -> Result<()> {
        ensure!(
            self.samples.len() == paths.len()
                && paths.iter().all(|path| self.samples.contains_key(path)),
            "measurement path set differs"
        );
        for &path in paths {
            for series in 0..SERIES {
                for index in 0..SAMPLES {
                    self.sample(path, series, index)?;
                }
            }
        }
        Ok(())
    }

    pub fn passes_reference(&self, paths: &[&'static str], policy: Policy) -> Result<bool> {
        self.validate(paths)?;
        let control_ok = self.summary("control")?.ratios.control_ok();
        let mut passed = control_ok;
        for &path in paths.iter().filter(|&&path| path != "control") {
            passed &= self.comparison(path, None, policy)?.status == Status::Pass;
        }
        Ok(passed)
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
