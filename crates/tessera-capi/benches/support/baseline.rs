//! Saved measurements for detecting slowdowns between library revisions.
//!
//! Read, validate, and write complete passing v2 runs with raw samples and an
//! environment fingerprint. Incompatible or incomplete runs are rejected and
//! existing files are never overwritten. This stores past timings; the separate
//! reference module supplies the scalar implementation timed in every run.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::measurement::{Measurement, Path as ReadingPath, SAMPLES, SERIES, Sample};

const HEADER: &str = "tessera-column-reader-baseline-v2";
const COLUMNS: &str = "case\tpath\tseries\tsample\tbefore_ns\tmeasured_ns\tafter_ns";

pub struct Baseline {
    pub environment: String,
    pub measurements: BTreeMap<String, Measurement>,
}

impl Baseline {
    pub fn read(path: &Path, environment: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&contents, environment)
    }

    pub fn parse(contents: &str, environment: &str) -> Result<Self> {
        let mut lines = contents.lines();
        ensure!(
            lines.next() == Some(HEADER),
            "unsupported baseline format; v2 raw samples are required, rerun --save-baseline with a new name"
        );
        ensure!(
            lines.next() == Some(environment),
            "baseline environment or benchmark definition differs"
        );
        ensure!(lines.next() == Some(COLUMNS), "invalid baseline columns");
        let mut measurements = BTreeMap::<String, Measurement>::new();
        for (line_index, line) in lines.enumerate() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(fields.len() == 7, "invalid baseline row {}", line_index + 4);
            validate_name(fields[0])?;
            let path = ReadingPath::parse(fields[1])?;
            let series = fields[2].parse().context("invalid series index")?;
            let index = fields[3].parse().context("invalid sample index")?;
            let sample = Sample {
                before_ns: fields[4].parse().context("invalid reference duration")?,
                measured_ns: fields[5].parse().context("invalid measured duration")?,
                after_ns: fields[6].parse().context("invalid reference duration")?,
            };
            measurements
                .entry(fields[0].to_owned())
                .or_default()
                .insert(path, series, index, sample)
                .with_context(|| format!("invalid baseline row {}", line_index + 4))?;
        }
        let baseline = Self {
            environment: environment.to_owned(),
            measurements,
        };
        baseline.validate()?;
        Ok(baseline)
    }

    pub fn validate_case_set(&self, names: &[String]) -> Result<()> {
        ensure!(
            self.measurements.len() == names.len()
                && names
                    .iter()
                    .all(|name| self.measurements.contains_key(name)),
            "baseline case set differs"
        );
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.environment.is_empty() && !self.environment.contains(['\n', '\r']),
            "baseline environment must be one nonempty line"
        );
        ensure!(!self.measurements.is_empty(), "empty baseline");
        for (name, measurement) in &self.measurements {
            validate_name(name)?;
            ensure!(
                measurement
                    .passes_reference()
                    .with_context(|| format!("incomplete baseline case {name}"))?,
                "baseline case {name} is FAIL or UNSTABLE; only complete passing runs may be saved"
            );
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let mut contents = format!("{HEADER}\n{}\n{COLUMNS}\n", self.environment);
        for (name, measurement) in &self.measurements {
            for reading in ReadingPath::ALL {
                for series in 0..SERIES {
                    for index in 0..SAMPLES {
                        let sample = measurement.sample(reading, series, index)?;
                        // Display preserves raw precision instead of rounded report values.
                        contents.push_str(&format!(
                            "{name}\t{}\t{series}\t{index}\t{}\t{}\t{}\n",
                            reading.name(),
                            sample.before_ns,
                            sample.measured_ns,
                            sample.after_ns
                        ));
                    }
                }
            }
        }
        std::fs::create_dir_all(path.parent().context("baseline has no parent directory")?)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| {
                format!(
                    "cannot create {}; existing baselines are never overwritten",
                    path.display()
                )
            })?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.chars().any(char::is_control),
        "invalid baseline case name"
    );
    Ok(())
}

pub fn path_for(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "baseline name must contain only letters, digits, '-' or '_'"
    );
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/column-reader-baselines")
        .join(format!("{name}.tsv")))
}

pub fn fingerprint(parts: &[&str]) -> u64 {
    parts
        .iter()
        .flat_map(|part| part.bytes().chain([0]))
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        })
}
