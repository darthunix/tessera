//! Raw timing samples and performance assessment, separate from the clock loop.
//!
//! Compare each reader with its surrounding reference runs and, optionally,
//! saved measurements. Three series distinguish consistent overhead from
//! unstable timings; a reference-versus-itself control detects measurement noise.
//! Keep every sample and apply the same 3% rules to full and diagnostic runs.

use anyhow::{Context, Result, ensure};
use std::fmt;

pub const SERIES: usize = 3;
pub const SAMPLES: usize = 15;
pub const LIMIT: f64 = 1.03;
const CONTROL_MIN: f64 = 0.97;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Fold,
    TryFold,
    Words,
    Control,
}

impl Path {
    pub const ALL: [Self; 4] = [Self::Fold, Self::TryFold, Self::Words, Self::Control];
    pub const READERS: [Self; 3] = [Self::Fold, Self::TryFold, Self::Words];

    pub fn name(self) -> &'static str {
        match self {
            Self::Fold => "fold",
            Self::TryFold => "try_fold",
            Self::Words => "words",
            Self::Control => "control",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|path| path.name() == name)
            .context("unknown measurement path")
    }
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
    pub change: Option<Ratios>,
    pub status: Status,
    pub reason: &'static str,
}

impl Assessment {
    pub fn new(current: Ratios, previous: Option<Ratios>, control_ok: bool) -> Self {
        let change = previous.map(|previous| current.relative_to(previous));
        let reference = if control_ok {
            current.status()
        } else {
            Status::Unstable
        };
        let previous = change.map(|change| {
            if control_ok {
                change.status()
            } else {
                Status::Unstable
            }
        });
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
            (Status::Unstable, "series range crosses a 3% limit")
        } else {
            (Status::Pass, "all comparison ranges within limits")
        };
        Self {
            reference,
            previous,
            change,
            status,
            reason,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Measurement {
    samples: [[[Option<Sample>; SAMPLES]; SERIES]; 4],
}

impl Default for Measurement {
    fn default() -> Self {
        Self {
            samples: [[[None; SAMPLES]; SERIES]; 4],
        }
    }
}

impl Measurement {
    pub fn insert(
        &mut self,
        path: Path,
        series: usize,
        index: usize,
        sample: Sample,
    ) -> Result<()> {
        sample.validate()?;
        let slot = self.samples[path as usize]
            .get_mut(series)
            .and_then(|series| series.get_mut(index))
            .context("sample index out of bounds")?;
        ensure!(slot.is_none(), "duplicate measurement sample");
        *slot = Some(sample);
        Ok(())
    }

    pub fn sample(&self, path: Path, series: usize, index: usize) -> Result<Sample> {
        self.samples[path as usize]
            .get(series)
            .and_then(|series| series.get(index))
            .copied()
            .flatten()
            .context("missing measurement sample")
    }

    pub fn summary(&self, path: Path) -> Result<Summary> {
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

    pub fn passes_reference(&self) -> Result<bool> {
        let control_ok = self.summary(Path::Control)?.ratios.control_ok();
        let mut passed = control_ok;
        for path in Path::READERS {
            passed &= self.summary(path)?.ratios.status() == Status::Pass;
        }
        Ok(passed)
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
