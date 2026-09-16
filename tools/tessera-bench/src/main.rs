//! Compare compatible source snapshots by PMU counters, several processes per side per case.
//! Only generated files under target/bench-runs are written. No Git publishing.
#![forbid(unsafe_code)]

mod cases;
mod report;
mod snapshot;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde::Serialize;
use serde_json::{Value, json};
use snapshot::{Snapshot, text};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    time::Instant,
};

#[derive(Parser, Debug)]
#[command(
    about = "Compare compatible Rust revisions by PMU counters (before/after per case, repeated).",
    after_help = "REF is a Git revision or WORKTREE. Defaults to both full benchmarks.\nFilters are substrings of operation ids; any match keeps an operation, and every\nselected case must keep its reference and a library path.\nEach case runs before/after --repeats times, interleaved; instructions come from any\nprocess, cycles from the minimum over all of them.\nBenchmark processes run through `sudo -n`: run `sudo -v` first.\nExit: 0 PASS, 1 regression (instructions, or cycles on long single-mode operations),\n2 UNSTABLE or invalid/incomplete run."
)]
struct Options {
    #[arg(long, value_name = "REF")]
    base: String,
    #[arg(long, value_name = "REF", default_value = "WORKTREE")]
    candidate: String,
    #[arg(long, value_parser = ["column_reader", "filter_int32"])]
    bench: Option<String>,
    #[arg(long, value_name = "SUBSTRING")]
    filter: Vec<String>,
    /// Processes per side per case; instructions need one, cycles benefit from more.
    #[arg(long, value_name = "N", default_value = "3")]
    repeats: NonZeroUsize,
}

struct Timing {
    started: Instant,
    measurement_started: Option<Instant>,
    measurement_seconds: f64,
}

#[derive(Serialize, PartialEq, Eq)]
struct Environment {
    arch: String,
    os: String,
    cpu: String,
    rustc: String,
    cargo: String,
    flags: BTreeMap<String, String>,
    cargo_config: BTreeMap<PathBuf, String>,
}

fn environment(root: &Path) -> Result<Environment> {
    let cpu = if cfg!(target_os = "macos") {
        text(Command::new("sysctl").args(["-n", "machdep.cpu.brand_string"]))?
    } else {
        fs::read_to_string("/proc/cpuinfo")?
            .lines()
            .find(|line| line.starts_with("model name") || line.starts_with("Hardware"))
            .context("cannot identify CPU")?
            .to_owned()
    };
    let flags = std::env::vars()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "RUSTFLAGS"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "RUSTDOCFLAGS"
                    | "RUSTC"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
                    | "RUSTUP_TOOLCHAIN"
                    | "CARGO_BUILD_TARGET"
                    | "CARGO_BUILD_RUSTFLAGS"
            ) || key.starts_with("CARGO_PROFILE_")
                || (key.starts_with("CARGO_TARGET_")
                    && (key.ends_with("_RUSTFLAGS") || key.ends_with("_LINKER")))
        })
        .collect();
    // Record hashes, never contents/credentials, of the shared Cargo config.
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cargo")));
    let mut cargo_config = BTreeMap::new();
    if let Some(dir) = cargo_home {
        for name in ["config", "config.toml"] {
            let path = dir.join(name);
            if path.exists() {
                cargo_config.insert(path.clone(), snapshot::digest(&fs::read(path)?));
            }
        }
    }
    Ok(Environment {
        arch: std::env::consts::ARCH.to_owned(),
        os: text(Command::new("uname").args(["-sr"]))?,
        cpu,
        flags,
        cargo_config,
        rustc: text(
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .current_dir(root)
                .arg("-vV"),
        )?,
        cargo: text(Command::new("cargo").current_dir(root).arg("-V"))?,
    })
}

fn create(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "cannot create {} (existing files are never overwritten)",
                path.display()
            )
        })
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer_pretty(create(path)?, value)?;
    Ok(())
}

/// Counters need root; the credentials must already be cached by `sudo -v`.
fn check_privileges() -> Result<()> {
    let status = Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("cannot start sudo")?;
    ensure!(
        status.success(),
        "PMU counters need root: run `sudo -v` first, then retry"
    );
    Ok(())
}

fn build(source: &Snapshot, bench: &str, artifacts: &Path, side: &str) -> Result<PathBuf> {
    println!("Building {side}/{bench}");
    let result = Command::new("cargo")
        .current_dir(&source.directory)
        .env("CARGO_TARGET_DIR", source.directory.join("target"))
        .args([
            "bench",
            "-p",
            "tessera-capi",
            "--bench",
            bench,
            "--locked",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .context("cannot start cargo bench")?;
    create(&artifacts.join(format!("{side}-{bench}-build.jsonl")))?.write_all(&result.stdout)?;
    create(&artifacts.join(format!("{side}-{bench}-build.log")))?.write_all(&result.stderr)?;
    ensure!(
        result.status.success(),
        "build failed: see {side}-{bench}-build.log in {}",
        artifacts.display()
    );
    let mut executable = None;
    for line in result
        .stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: Value = serde_json::from_slice(line).context("invalid Cargo JSON")?;
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == bench
            && let Some(path) = message["executable"].as_str()
        {
            executable = Some(PathBuf::from(path));
        }
    }
    executable.context("Cargo did not produce a benchmark executable")
}

/// A measuring command: the benchmark under `sudo -n`, writing `output`
/// and measuring only `ids`.
fn benchmark(
    executable: &Path,
    output: &Path,
    ids: &BTreeSet<String>,
    privileged: bool,
) -> Command {
    let mut cmd = if privileged {
        let mut sudo = Command::new("sudo");
        sudo.arg("-n").arg("--").arg(executable);
        sudo
    } else {
        Command::new(executable)
    };
    cmd.arg("--output").arg(output);
    for id in ids {
        cmd.arg("--only").arg(id);
    }
    cmd.stdin(Stdio::null());
    cmd
}

fn listing(executable: &Path) -> Result<BTreeSet<String>> {
    report::listed(&text(Command::new(executable).arg("--list"))?)
}

fn collect(
    executable: &Path,
    directory: &Path,
    name: &str,
    expected: &BTreeSet<String>,
) -> Result<report::Run> {
    let output = directory.join(format!("{name}.jsonl"));
    let log = create(&directory.join(format!("{name}.log")))?;
    let status = benchmark(executable, &output, expected, true)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .status()?;
    ensure!(
        status.success(),
        "benchmark failed: {name}; see {}",
        directory.join(format!("{name}.log")).display()
    );
    report::load(&output, expected).with_context(|| format!("invalid measurement: {name}"))
}

fn compare(repo: &Path, root: &Path, options: &Options, timing: &mut Timing) -> Result<u8> {
    check_privileges()?;
    let before = Snapshot::capture(repo, &options.base, root.join("before"))?;
    // WORKTREE/WORKTREE captures one instant, even if files change while building.
    let after = if options.base == options.candidate {
        before.duplicate(root.join("after"))?
    } else {
        Snapshot::capture(repo, &options.candidate, root.join("after"))?
    };
    before.check_compatible(&after)?;
    let env = environment(&before.directory)?;
    ensure!(
        env == environment(&after.directory)?,
        "compiler or environment differs between snapshots"
    );
    save(
        &root.join("sources.json"),
        &json!({"before":before,"after":after,"environment":env,
        "started_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis(),
        "utility_sha256":snapshot::digest(&fs::read(std::env::current_exe()?)?),
        "bench":options.bench,
        "mode":if options.filter.is_empty() {"full"} else {"diagnostic"},
        "filter":options.filter,"order_unit":"case","order":cases::ORDER.map(|(name, _)| name),
        "repeats":options.repeats.get(),
        "measurement":{"metric":"pmu counters per call","blocks":report::BLOCKS,
            "instruction_limit":report::INSTRUCTION_LIMIT,
            "instruction_spread_limit":report::INSTRUCTION_SPREAD_LIMIT,
            "cycle_warning":report::CYCLE_WARNING,"short_cycles":report::SHORT_CYCLES,
            "short_cycle_warning":report::SHORT_CYCLE_WARNING,
            "cycle_fail":report::CYCLE_FAIL,"cycle_fail_cycles":report::CYCLE_FAIL_CYCLES,
            "modes_limit":report::MODES_LIMIT}}),
    )?;
    let benches: Vec<_> = options.bench.as_deref().map_or_else(
        || vec!["column_reader", "filter_int32"],
        |bench| vec![bench],
    );
    // Finish every build and listing before starting any measuring process.
    let mut binaries = BTreeMap::new();
    let mut cases = Vec::new();
    let mut identities = BTreeMap::new();
    for bench in benches {
        let a = build(&before, bench, root, "before")?;
        let b = build(&after, bench, root, "after")?;
        let listed = listing(&a)?;
        ensure!(listed == listing(&b)?, "benchmark case sets differ");
        let expected = report::listed(
            &cases::select(listed, &options.filter)
                .into_iter()
                .collect::<Vec<_>>()
                .join("\n"),
        )?;
        for (side, binary) in [("before", &a), ("after", &b)] {
            identities.insert(
                format!("{side}/{bench}"),
                json!({"path":binary,"sha256":snapshot::digest(&fs::read(binary)?)}),
            );
        }
        let selected = cases::group(bench, expected)?;
        save(&root.join(format!("{bench}-cases.json")), &selected)?;
        for case in &selected {
            fs::create_dir(root.join(&case.directory))?;
        }
        cases.extend(selected);
        binaries.insert(bench.to_owned(), [a, b]);
    }
    save(&root.join("binaries.json"), &identities)?;
    let mut report = create(&root.join("report.txt"))?;
    writeln!(
        report,
        "{}; baseline={}, candidate={}",
        if options.filter.is_empty() {
            "FULL RUN"
        } else {
            "DIAGNOSTIC: selected cases only"
        },
        before.revision,
        after.revision
    )?;
    let measurement_started = Instant::now();
    timing.measurement_started = Some(measurement_started);
    // Repeats interleave before/after per case so that drift in core state is
    // shared by both sides; process k of every case forms one run per side.
    let repeats = options.repeats.get();
    let mut measured: BTreeMap<String, [Vec<report::Run>; 2]> = BTreeMap::new();
    for case in &cases {
        let runs = measured
            .entry(case.bench.clone())
            .or_insert_with(|| std::array::from_fn(|_| vec![report::Run::new(); repeats]));
        for repeat in 0..repeats {
            for (name, side) in cases::ORDER {
                let label = format!("{name}{}", repeat + 1);
                println!("Case {}/{label}: {}", case.directory, case.name);
                let collected = collect(
                    &binaries[&case.bench][side],
                    &root.join(&case.directory),
                    &label,
                    &case.paths,
                )
                .with_context(|| format!("{}: {label}", case.name))?;
                let run = runs[side].get_mut(repeat).context("missing repeat")?;
                for (id, entry) in collected {
                    ensure!(
                        run.insert(id.clone(), entry).is_none(),
                        "duplicate benchmark result: {id}"
                    );
                }
            }
        }
    }
    timing.measurement_seconds = measurement_started.elapsed().as_secs_f64();
    let mut outcome = report::Status::Pass;
    for (bench, runs) in &measured {
        let mut section = Vec::new();
        writeln!(section, "\n{bench}")?;
        let sides = [report::aggregate(&runs[0])?, report::aggregate(&runs[1])?];
        let status = report::print(&mut section, &sides)?;
        report.write_all(&section)?;
        print!("{}", String::from_utf8(section)?);
        outcome = outcome.combine(status);
    }
    ensure!(
        env == environment(&before.directory)?,
        "environment changed during measurement"
    );
    save(
        &root.join("result.json"),
        &json!({"status":outcome.label(),"exit_code":outcome.exit_code()}),
    )?;
    Ok(outcome.exit_code())
}

fn run(options: Options) -> Result<u8> {
    let mut timing = Timing {
        started: Instant::now(),
        measurement_started: None,
        measurement_seconds: 0.,
    };
    let repo = PathBuf::from(text(
        Command::new("git").args(["rev-parse", "--show-toplevel"]),
    )?);
    let parent = repo.join("target/bench-runs");
    fs::create_dir_all(&parent)?;
    let root = tempfile::Builder::new()
        .prefix("compare-")
        .tempdir_in(parent)?
        .keep();
    println!("Artifacts: {}", root.display());
    println!(
        "{}; no retries, no discarded measurements.",
        if options.filter.is_empty() {
            "FULL RUN"
        } else {
            "DIAGNOSTIC RUN"
        }
    );
    let result = compare(&repo, &root, &options, &mut timing);
    let finished = Instant::now();
    let preparation_seconds = timing
        .measurement_started
        .unwrap_or(finished)
        .duration_since(timing.started)
        .as_secs_f64();
    let total_seconds = finished.duration_since(timing.started).as_secs_f64();
    save(
        &root.join("timings.json"),
        &json!({"preparation_seconds":preparation_seconds,
            "measurement_seconds":timing.measurement_seconds,"total_seconds":total_seconds}),
    )?;
    println!(
        "Time: preparation={preparation_seconds:.1}s, measurements={:.1}s, total={total_seconds:.1}s",
        timing.measurement_seconds
    );
    if let Err(error) = &result {
        create(&root.join("error.txt"))?.write_all(format!("{error:#}\n").as_bytes())?;
    }
    result
}

fn main() -> ExitCode {
    match run(Options::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ERROR: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_requires_a_baseline_and_bounds_the_selected_benchmarks() {
        assert!(Options::try_parse_from(["tessera-bench"]).is_err());
        let options = Options::try_parse_from(["tessera-bench", "--base", "HEAD"]).unwrap();
        assert_eq!(options.candidate, "WORKTREE");
        assert!(options.bench.is_none());
        assert!(options.filter.is_empty());
        assert_eq!(options.repeats.get(), 3);
        assert!(
            Options::try_parse_from(["tessera-bench", "--base", "HEAD", "--repeats", "0"]).is_err()
        );
        assert_eq!(
            Options::try_parse_from(["tessera-bench", "--base", "HEAD", "--repeats", "1"])
                .unwrap()
                .repeats
                .get(),
            1
        );
        let options = Options::try_parse_from([
            "tessera-bench",
            "--base",
            "HEAD",
            "--filter",
            "/dense/",
            "--filter",
            "/datum/",
        ])
        .unwrap();
        assert_eq!(options.filter, ["/dense/", "/datum/"]);
        assert!(
            Options::try_parse_from(["tessera-bench", "--base", "HEAD", "--bench", "unknown"])
                .is_err()
        );
        assert!(
            Options::try_parse_from(["tessera-bench", "--base", "HEAD", "--jobs", "2"]).is_err()
        );
    }

    #[test]
    fn benchmark_commands_select_operations_and_can_run_under_sudo() {
        let ids = BTreeSet::from(["b/x/fold".to_owned(), "b/x/reference".to_owned()]);
        let cmd = benchmark(Path::new("bench"), Path::new("out.jsonl"), &ids, false);
        assert_eq!(cmd.get_program(), "bench");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                "--output",
                "out.jsonl",
                "--only",
                "b/x/fold",
                "--only",
                "b/x/reference"
            ]
        );
        let cmd = benchmark(Path::new("bench"), Path::new("out.jsonl"), &ids, true);
        assert_eq!(cmd.get_program(), "sudo");
        assert_eq!(
            cmd.get_args().take(3).collect::<Vec<_>>(),
            ["-n", "--", "bench"]
        );
    }

    #[test]
    fn saved_files_are_never_overwritten() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("run.json");
        save(&file, &json!({"original":true}))?;
        assert!(save(&file, &json!({"original":false})).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(file)?)?,
            json!({"original":true})
        );
        Ok(())
    }
}
