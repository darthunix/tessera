//! Validate Criterion artifacts and apply project limits to its mean estimates.
//! No resampling, outlier removal or reference normalization happens here.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Unstable,
}

impl Status {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Unstable => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Unstable => "UNSTABLE",
        }
    }
    pub fn combine(self, other: Self) -> Self {
        if self == Self::Fail || other == Self::Fail {
            Self::Fail
        } else if self == Self::Unstable || other == Self::Unstable {
            Self::Unstable
        } else {
            Self::Pass
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub point: f64,
    pub low: f64,
    pub high: f64,
}

impl Bounds {
    fn envelope(self, other: Self) -> Self {
        Self {
            point: self.point / 2. + other.point / 2.,
            low: self.low.min(other.low),
            high: self.high.max(other.high),
        }
    }
    fn relative_to(self, old: Self) -> Self {
        Self {
            point: self.point / old.point,
            low: self.low / old.high,
            high: self.high / old.low,
        }
    }
    fn difference(self, old: Self) -> Self {
        Self {
            point: self.point - old.point,
            low: self.low - old.high,
            high: self.high - old.low,
        }
    }
    fn repeatable(self, other: Self) -> bool {
        let ratio = other.relative_to(self);
        ratio.low >= 0.97 && ratio.high <= 1.03
    }
    fn status(self, old: Self, filter: bool) -> Status {
        let ratio = self.relative_to(old);
        let delta = self.difference(old);
        if ratio.high <= 1.03 || (filter && delta.high <= 1.) {
            Status::Pass
        } else if ratio.low > 1.03 && (!filter || delta.low > 1.) {
            Status::Fail
        } else {
            Status::Unstable
        }
    }
}

#[derive(Deserialize)]
struct Estimate {
    point_estimate: f64,
    confidence_interval: Confidence,
}
#[derive(Deserialize)]
struct Confidence {
    confidence_level: f64,
    lower_bound: f64,
    upper_bound: f64,
}
#[derive(Deserialize)]
struct Estimates {
    mean: Estimate,
}
#[derive(Deserialize)]
struct Sample {
    iters: Vec<f64>,
    times: Vec<f64>,
}
#[derive(Deserialize)]
struct Identity {
    full_id: String,
    title: String,
    group_id: String,
    function_id: String,
}

pub struct Entry {
    pub group: String,
    pub path: String,
    pub time: Bounds,
}
pub type Run = BTreeMap<String, Entry>;

fn json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path)?).with_context(|| format!("invalid {}", path.display()))
}

pub fn listed(text: &str) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let name = line
            .strip_suffix(": benchmark")
            .context("unsupported Criterion listing")?;
        ensure!(
            !name.is_empty() && names.insert(name.to_owned()),
            "duplicate/empty benchmark name"
        );
    }
    ensure!(!names.is_empty(), "no benchmark cases matched");
    // Filters select cases, not an isolated path without its scalar reference.
    for name in &names {
        let (group, path) = name.rsplit_once('/').context("invalid benchmark name")?;
        ensure!(
            names.contains(&format!("{group}/reference")),
            "filter must include the reference for {group}"
        );
        ensure!(
            path != "reference"
                || names.iter().any(|other| other != name
                    && other.rsplit_once('/').is_some_and(|(g, _)| g == group)),
            "filter selected only a reference: {group}"
        );
    }
    Ok(names)
}

pub fn load(directory: &Path, expected: &BTreeSet<String>) -> Result<Run> {
    let mut run = Run::new();
    let mut seen = BTreeSet::new();
    for path in crate::snapshot::files(directory)? {
        if !path.ends_with("new/benchmark.json") {
            continue;
        }
        let parent = directory.join(path.parent().unwrap());
        let id: Identity = json(&parent.join("benchmark.json"))?;
        ensure!(
            expected.contains(&id.title) && seen.insert(id.title),
            "unexpected or duplicate benchmark: {}",
            id.full_id
        );
        ensure!(
            id.full_id == format!("{}/{}", id.group_id, id.function_id),
            "inconsistent benchmark identity"
        );
        ensure!(
            matches!(
                id.function_id.as_str(),
                "fold" | "try_fold" | "words" | "scalar" | "reference"
            ),
            "unsupported measured path"
        );
        let estimate: Estimates = json(&parent.join("estimates.json"))?;
        let ci = estimate.mean.confidence_interval;
        let time = Bounds {
            point: estimate.mean.point_estimate,
            low: ci.lower_bound,
            high: ci.upper_bound,
        };
        ensure!(
            ci.confidence_level == 0.99
                && [time.point, time.low, time.high]
                    .iter()
                    .all(|v| v.is_finite() && *v > 0.)
                && time.low <= time.point
                && time.point <= time.high,
            "invalid mean estimate for {}",
            id.full_id
        );
        let sample: Sample = json(&parent.join("sample.json"))?;
        ensure!(
            sample.iters.len() == 100
                && sample.times.len() == 100
                && sample
                    .iters
                    .iter()
                    .chain(&sample.times)
                    .all(|v| v.is_finite() && *v > 0.),
            "incomplete/invalid samples for {}",
            id.full_id
        );
        ensure!(
            run.insert(
                id.full_id,
                Entry {
                    group: id.group_id,
                    path: id.function_id,
                    time
                }
            )
            .is_none(),
            "duplicate full benchmark ID"
        );
    }
    ensure!(
        !expected.is_empty() && seen == *expected,
        "missing Criterion results: {:?}",
        expected.difference(&seen).collect::<Vec<_>>()
    );
    Ok(run)
}

/// Runs are A1, B1, B2, A2. Bounds enclose the two independently measured
/// intervals; they are deliberately not described as a joint 99% interval.
pub fn print(out: &mut impl Write, runs: &[Run; 4]) -> Result<Status> {
    let names: Vec<_> = runs[0].keys().collect();
    ensure!(
        !names.is_empty()
            && runs
                .iter()
                .all(|run| run.keys().collect::<Vec<_>>() == names),
        "run case sets differ"
    );
    writeln!(
        out,
        "Mean times; bounds enclose both runs' Criterion 99% intervals, not a joint confidence interval."
    )?;
    writeln!(
        out,
        "History uses absolute library times. Reference overhead is informational only."
    )?;
    let mut status = Status::Pass;
    let mut counts = [0; 3];
    for name in names {
        let entry = &runs[0][name];
        if entry.path == "reference" {
            continue;
        }
        let reference = format!("{}/reference", entry.group);
        let values: Vec<_> = runs
            .iter()
            .map(|run| {
                let item = &run[name];
                ensure!(
                    item.group == entry.group && item.path == entry.path,
                    "identity changed between runs"
                );
                Ok((
                    item.time,
                    run.get(&reference).context("missing reference")?.time,
                ))
            })
            .collect::<Result<_>>()?;
        let controls = [
            ("before-library", values[0].0, values[3].0),
            ("after-library", values[1].0, values[2].0),
            ("before-reference", values[0].1, values[3].1),
            ("after-reference", values[1].1, values[2].1),
        ];
        let stable = controls.iter().all(|(_, a, b)| a.repeatable(*b));
        let old = values[0].0.envelope(values[3].0);
        let new = values[1].0.envelope(values[2].0);
        let ratio = new.relative_to(old);
        let delta = new.difference(old);
        let ref_old = values[0].1.envelope(values[3].1);
        let ref_new = values[1].1.envelope(values[2].1);
        let outcome = if stable {
            new.status(old, entry.group.starts_with("filter_int32/"))
        } else {
            Status::Unstable
        };
        status = status.combine(outcome);
        counts[outcome.exit_code() as usize] += 1;
        writeln!(
            out,
            "{} {name}: before={:.3}ns after={:.3}ns change={:+.2}% [{:+.2}%, {:+.2}%] delta={:+.3}ns [{:+.3}, {:+.3}]; repeatability={}",
            outcome.label(),
            old.point,
            new.point,
            (ratio.point - 1.) * 100.,
            (ratio.low - 1.) * 100.,
            (ratio.high - 1.) * 100.,
            delta.point,
            delta.low,
            delta.high,
            if stable { "PASS" } else { "UNSTABLE" }
        )?;
        writeln!(
            out,
            "  REFERENCE before={:.3}ns after={:.3}ns; library/reference={:.3}x delta={:+.3}ns (informational)",
            ref_old.point,
            ref_new.point,
            new.point / ref_new.point,
            new.point - ref_new.point
        )?;
        for (label, a, b) in controls {
            if !a.repeatable(b) {
                let ratio = b.relative_to(a);
                writeln!(
                    out,
                    "  UNSTABLE {label}: repeat change={:+.2}% [{:+.2}%, {:+.2}%], requires [-3%, +3%]",
                    (ratio.point - 1.) * 100.,
                    (ratio.low - 1.) * 100.,
                    (ratio.high - 1.) * 100.
                )?;
            }
        }
    }
    ensure!(
        counts.iter().sum::<usize>() != 0,
        "no library paths measured"
    );
    writeln!(
        out,
        "{}: {} PASS, {} FAIL, {} UNSTABLE library paths",
        status.label(),
        counts[0],
        counts[1],
        counts[2]
    )?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn exact(ns: f64) -> Bounds {
        Bounds {
            point: ns,
            low: ns,
            high: ns,
        }
    }

    #[test]
    fn limits_are_inclusive_and_filter_requires_both_losses() {
        for (old, new, filter, expected) in [
            (100., 103., false, Status::Pass),
            (100., 104., false, Status::Fail),
            (5., 5.5, true, Status::Pass),
            (5., 6., true, Status::Pass),
            (5., 6.01, true, Status::Fail),
            (100., 103., true, Status::Pass),
        ] {
            assert_eq!(exact(new).status(exact(old), filter), expected);
        }
        assert_eq!(
            Bounds {
                point: 104.,
                low: 102.,
                high: 106.
            }
            .status(exact(100.), false),
            Status::Unstable
        );
        assert!(exact(100.).repeatable(exact(97.)));
        assert!(exact(100.).repeatable(exact(103.)));
        assert!(!exact(100.).repeatable(exact(104.)));
        assert_eq!(Status::Fail.combine(Status::Unstable).exit_code(), 1);
    }

    fn run(library: f64, reference: f64) -> Run {
        [("scalar", library), ("reference", reference)]
            .into_iter()
            .map(|(path, ns)| {
                (
                    format!("filter_int32/case/{path}"),
                    Entry {
                        group: "filter_int32/case".into(),
                        path: path.into(),
                        time: exact(ns),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn history_is_independent_of_reference_overhead_and_speed() -> Result<()> {
        let mut out = Vec::new();
        assert_eq!(
            print(
                &mut out,
                &[
                    run(523., 800.),
                    run(608., 800.),
                    run(608., 800.),
                    run(523., 800.)
                ]
            )?,
            Status::Fail
        );
        assert_eq!(
            print(
                &mut out,
                &[
                    run(200., 100.),
                    run(200., 100.),
                    run(200., 100.),
                    run(200., 100.)
                ]
            )?,
            Status::Pass
        );
        assert_eq!(
            print(
                &mut out,
                &[
                    run(100., 100.),
                    run(110., 110.),
                    run(110., 110.),
                    run(100., 100.)
                ]
            )?,
            Status::Fail
        );
        assert_eq!(
            print(
                &mut out,
                &[
                    run(100., 100.),
                    run(120., 100.),
                    run(120., 100.),
                    run(110., 100.)
                ]
            )?,
            Status::Unstable
        );
        assert_eq!(
            print(
                &mut out,
                &[
                    run(100., 100.),
                    run(120., 100.),
                    run(120., 110.),
                    run(100., 100.)
                ]
            )?,
            Status::Unstable
        );
        assert!(
            print(
                &mut out,
                &[
                    run(100., 100.),
                    Run::new(),
                    run(100., 100.),
                    run(100., 100.)
                ]
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn artifacts_require_complete_finite_samples_and_matching_identities() -> Result<()> {
        use serde_json::json;
        let temp = tempfile::tempdir()?;
        let expected = listed(
            "filter_int32/case/scalar: benchmark\nfilter_int32/case/reference: benchmark\n",
        )?;
        for path in ["scalar", "reference"] {
            let dir = temp.path().join(path).join("new");
            fs::create_dir_all(&dir)?;
            let name = format!("filter_int32/case/{path}");
            fs::write(dir.join("benchmark.json"), json!({"full_id":name,"title":name,"group_id":"filter_int32/case","function_id":path}).to_string())?;
            fs::write(dir.join("estimates.json"), json!({"mean":{"point_estimate":10.,"confidence_interval":{"confidence_level":0.99,"lower_bound":9.99,"upper_bound":10.01}}}).to_string())?;
            fs::write(
                dir.join("sample.json"),
                json!({"iters":vec![100.;100],"times":vec![1000.;100]}).to_string(),
            )?;
        }
        assert_eq!(load(temp.path(), &expected)?.len(), 2);
        for bad in [
            json!({"iters":[1.],"times":[1.]}),
            json!({"iters":vec![1.;100],"times":vec![0.;100]}),
        ] {
            fs::write(temp.path().join("scalar/new/sample.json"), bad.to_string())?;
            assert!(load(temp.path(), &expected).is_err());
        }
        assert!(listed("").is_err());
        assert!(listed("case/scalar: benchmark\n").is_err());
        assert!(listed("case/reference: benchmark\n").is_err());
        assert!(load(temp.path(), &BTreeSet::from(["missing".to_owned()])).is_err());
        Ok(())
    }
}
