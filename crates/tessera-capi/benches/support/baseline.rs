//! Raw results and passing baselines, using one TSV reader and writer.
//!
//! Results may be diagnostic, FAIL or UNSTABLE, but every selected case must be
//! complete. Baselines additionally require a full PASS. Distinct headers and
//! directories prevent results from being read as baselines. Neither overwrites
//! existing files; the independent timed implementation lives in reference.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::measurement::{Measurement, Policy, SAMPLES, SERIES, Sample};

/// Benchmark identity, measured paths and acceptance policy, not a release version.
#[derive(Clone, Copy, Debug)]
pub struct Definition {
    pub name: &'static str,
    pub paths: &'static [&'static str],
    pub policy: Policy,
}

impl Definition {
    pub fn header(self, kind: Kind) -> String {
        let suffix = match kind {
            Kind::Baseline => "baseline",
            Kind::Results => "results",
        };
        format!("tessera-{}-{suffix}", self.name.replace('_', "-"))
    }
    pub fn directory(self, kind: Kind) -> String {
        let suffix = match kind {
            Kind::Baseline => "baselines",
            Kind::Results => "results",
        };
        format!("{}-{suffix}", self.name.replace('_', "-"))
    }
}
const COLUMNS: &str = "case\tpath\tseries\tsample\tbefore_ns\tmeasured_ns\tafter_ns";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Baseline,
    Results,
}

pub struct Run {
    pub kind: Kind,
    pub definition: Definition,
    pub environment: String,
    pub diagnostic: bool,
    pub previous: Option<String>,
    pub measurements: BTreeMap<String, Measurement>,
}

impl Run {
    pub fn read(
        path: &Path,
        environment: &str,
        definition: Definition,
        kind: Kind,
    ) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&contents, environment, definition, kind)
    }

    pub fn parse(
        contents: &str,
        environment: &str,
        definition: Definition,
        kind: Kind,
    ) -> Result<Self> {
        let mut lines = contents.lines();
        ensure!(
            lines.next() == Some(definition.header(kind).as_str()),
            "unsupported saved-run format; expected {}, rerun with a new name",
            definition.header(kind)
        );
        ensure!(
            lines.next() == Some(environment),
            "saved-run environment or benchmark definition differs"
        );
        let metadata: Vec<_> = lines
            .next()
            .context("missing run metadata")?
            .split('\t')
            .collect();
        ensure!(
            metadata.len() == 3 && metadata[0] == "mode",
            "invalid run metadata"
        );
        let diagnostic = match metadata[1] {
            "full" => false,
            "diagnostic" => true,
            _ => anyhow::bail!("invalid run mode"),
        };
        let previous = (!metadata[2].is_empty()).then(|| metadata[2].to_owned());
        ensure!(lines.next() == Some(COLUMNS), "invalid saved-run columns");
        let mut measurements = BTreeMap::<String, Measurement>::new();
        for (line_index, line) in lines.enumerate() {
            let fields: Vec<_> = line.split('\t').collect();
            ensure!(
                fields.len() == 7,
                "invalid saved-run row {}",
                line_index + 5
            );
            validate_name(fields[0])?;
            let path = definition
                .paths
                .iter()
                .copied()
                .find(|&path| path == fields[1])
                .context("unknown measurement path")?;
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
                .with_context(|| format!("invalid saved-run row {}", line_index + 5))?;
        }
        let run = Self {
            kind,
            definition,
            environment: environment.to_owned(),
            diagnostic,
            previous,
            measurements,
        };
        run.validate()?;
        Ok(run)
    }

    pub fn validate_case_set(&self, names: &[String]) -> Result<()> {
        ensure!(
            self.measurements.len() == names.len()
                && names
                    .iter()
                    .all(|name| self.measurements.contains_key(name)),
            "saved-run case set differs"
        );
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.environment.is_empty() && !self.environment.contains(['\n', '\r']),
            "saved-run environment must be one nonempty line"
        );
        ensure!(!self.measurements.is_empty(), "empty saved run");
        if let Some(name) = &self.previous {
            validate_file_name(name)?;
        }
        ensure!(
            self.kind != Kind::Baseline || !self.diagnostic,
            "diagnostic runs cannot be baselines"
        );
        for (name, measurement) in &self.measurements {
            validate_name(name)?;
            measurement
                .validate(self.definition.paths)
                .with_context(|| format!("incomplete saved-run case {name}"))?;
            if self.kind == Kind::Baseline {
                ensure!(
                    measurement.passes_reference(self.definition.paths, self.definition.policy)?,
                    "baseline case {name} is FAIL or UNSTABLE; only complete passing runs may be baselines"
                );
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let mut contents = format!(
            "{}\n{}\nmode\t{}\t{}\n{COLUMNS}\n",
            self.definition.header(self.kind),
            self.environment,
            if self.diagnostic {
                "diagnostic"
            } else {
                "full"
            },
            self.previous.as_deref().unwrap_or("")
        );
        for (name, measurement) in &self.measurements {
            for &reading in self.definition.paths {
                for series in 0..SERIES {
                    for index in 0..SAMPLES {
                        let sample = measurement.sample(reading, series, index)?;
                        // Display preserves raw precision instead of rounded report values.
                        contents.push_str(&format!(
                            "{name}\t{}\t{series}\t{index}\t{}\t{}\t{}\n",
                            reading, sample.before_ns, sample.measured_ns, sample.after_ns
                        ));
                    }
                }
            }
        }
        std::fs::create_dir_all(path.parent().context("saved run has no parent directory")?)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| {
                format!(
                    "cannot create {}; existing saved runs are never overwritten",
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
        "invalid saved-run case name"
    );
    Ok(())
}

fn validate_file_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "saved-run name must contain only letters, digits, '-' or '_'"
    );
    Ok(())
}

pub fn path_for(name: &str, definition: Definition, kind: Kind) -> Result<PathBuf> {
    validate_file_name(name)?;
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(definition.directory(kind))
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

fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program).args(args).output()?;
    ensure!(output.status.success(), "{program} failed");
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

pub fn environment(sources: &[&str]) -> Result<String> {
    let cpu = if cfg!(target_os = "macos") {
        command("sysctl", &["-n", "machdep.cpu.brand_string"])?
    } else {
        std::fs::read_to_string("/proc/cpuinfo")?
            .lines()
            .find(|line| line.starts_with("model name") || line.starts_with("Hardware"))
            .context("cannot identify CPU for a comparable baseline")?
            .to_owned()
    };
    // Shared code is part of every definition, including orchestration and help.
    let shared = [
        include_str!("mod.rs"),
        include_str!("runner.rs"),
        include_str!("sampling.rs"),
        include_str!("report.rs"),
        include_str!("baseline.rs"),
        include_str!("options.rs"),
        include_str!("measurement.rs"),
        include_str!("fixture.rs"),
        include_str!("reference.rs"),
        include_str!("../../../../Cargo.toml"),
        include_str!("../../../../Cargo.lock"),
        include_str!("../../Cargo.toml"),
        include_str!("../../../tessera-core/Cargo.toml"),
        include_str!("../../../tessera-kernels/Cargo.toml"),
    ];
    let definition = fingerprint(&[sources, &shared].concat());
    Ok(format!(
        "{}|{}|{}|{}|{:?}|{:?}|{definition:016x}",
        std::env::consts::ARCH,
        command("uname", &["-sr"])?,
        cpu,
        command("rustc", &["-vV"])?,
        std::env::var("RUSTFLAGS").unwrap_or_default(),
        std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default()
    ))
}
