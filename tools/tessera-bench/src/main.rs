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
    after_help = "REF is a Git revision or WORKTREE. Defaults to both full benchmarks.\nFilters are substrings of operation ids; any match keeps an operation, and every\nselected case must keep its reference and a library path.\nEach case runs before/after --repeats times, interleaved; instructions come from any\nprocess, cycles from the minimum over all of them.\nBenchmark processes run through `sudo -n`: run `sudo -v` first, or allow the\nbenchmark executables without a password in sudoers (see benches/README.md).\nExit: 0 PASS, 1 regression (instructions, or cycles on long single-mode operations),\n2 UNSTABLE or invalid/incomplete run."
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

/// Counters need root. `sudo -l` succeeds without a prompt when `sudo -v`
/// has cached the credentials or sudoers grants the user any command without
/// a password; the exact benchmark commands are checked again after the
/// build, before any measurement.
fn check_privileges() -> Result<()> {
    let status = Command::new("sudo")
        .args(["-n", "-l"])
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

/// The benchmark executable, through `sudo -n` when `privileged`.
fn launch(executable: &Path, privileged: bool) -> Command {
    let mut cmd = if privileged {
        let mut sudo = Command::new("sudo");
        sudo.arg("-n").arg("--").arg(executable);
        sudo
    } else {
        Command::new(executable)
    };
    cmd.stdin(Stdio::null());
    cmd
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

/// A measuring command writing `output` and measuring only `ids`.
fn benchmark(
    executable: &Path,
    output: &Path,
    ids: &BTreeSet<String>,
    privileged: bool,
) -> Command {
    let mut cmd = launch(executable, privileged);
    cmd.arg("--output").arg(output);
    for id in ids {
        cmd.arg("--only").arg(id);
    }
    cmd
}

/// Operation ids; taken the way the measurements run, so that a missing
/// privilege fails here rather than in the first measurement.
fn listing(executable: &Path, privileged: bool) -> Result<BTreeSet<String>> {
    report::listed(&text(launch(executable, privileged).arg("--list"))?)
}

fn collect(
    executable: &Path,
    directory: &Path,
    name: &str,
    expected: &BTreeSet<String>,
    privileged: bool,
) -> Result<report::Run> {
    let output = directory.join(format!("{name}.jsonl"));
    let log = create(&directory.join(format!("{name}.log")))?;
    let status = benchmark(executable, &output, expected, privileged)
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

/// Per benchmark program, the before and after executables.
type Binaries = BTreeMap<String, [PathBuf; 2]>;
/// Per benchmark program, the runs of each side: process k of every case
/// forms run k of its side.
type Measured = BTreeMap<String, [Vec<report::Run>; 2]>;

/// Measure every case in its own directory under `root`: `repeats` pairs of
/// processes, interleaved before/after so that drift in core state is shared
/// by both sides.
fn measure(
    cases: &[cases::Case],
    binaries: &Binaries,
    root: &Path,
    repeats: usize,
    privileged: bool,
) -> Result<Measured> {
    let mut measured = Measured::new();
    for case in cases {
        let directory = root.join(&case.directory);
        fs::create_dir(&directory)?;
        let runs = measured
            .entry(case.bench.clone())
            .or_insert_with(|| std::array::from_fn(|_| vec![report::Run::new(); repeats]));
        let executables = binaries
            .get(&case.bench)
            .with_context(|| format!("no executables for {}", case.bench))?;
        for repeat in 0..repeats {
            for (name, side) in cases::ORDER {
                let label = format!("{name}{}", repeat + 1);
                println!("Case {}/{label}: {}", case.directory, case.name);
                let collected = collect(
                    &executables[side],
                    &directory,
                    &label,
                    &case.paths,
                    privileged,
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
    Ok(measured)
}

/// The report of every benchmark program and the combined status.
fn summarize(measured: &Measured) -> Result<(report::Status, String)> {
    let mut outcome = report::Status::Pass;
    let mut text = Vec::new();
    for (bench, runs) in measured {
        writeln!(text, "\n{bench}")?;
        let sides = [report::aggregate(&runs[0])?, report::aggregate(&runs[1])?];
        outcome = outcome.combine(report::print(&mut text, &sides)?);
    }
    Ok((outcome, String::from_utf8(text)?))
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
    let mut binaries = Binaries::new();
    let mut cases = Vec::new();
    let mut identities = BTreeMap::new();
    for bench in benches {
        let a = build(&before, bench, root, "before")?;
        let b = build(&after, bench, root, "after")?;
        let listed = listing(&a, true)?;
        ensure!(listed == listing(&b, true)?, "benchmark case sets differ");
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
    let measured = measure(&cases, &binaries, root, options.repeats.get(), true)?;
    timing.measurement_seconds = measurement_started.elapsed().as_secs_f64();
    let (outcome, summary) = summarize(&measured)?;
    report.write_all(summary.as_bytes())?;
    print!("{summary}");
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
    fn listings_run_like_measurements() {
        let cmd = launch(Path::new("bench"), true);
        assert_eq!(cmd.get_program(), "sudo");
        assert_eq!(launch(Path::new("bench"), false).get_program(), "bench");
    }

    /// A benchmark stand-in: answers `--list` with `ids`, writes one record
    /// per `--only` id from `records` (instructions and cycles per block, 10
    /// calls per block), and appends every invocation to `calls.log` next to
    /// itself. Exits with `failure` instead when it is nonzero.
    fn fake_benchmark(
        dir: &Path,
        name: &str,
        records: &[(&str, u64, [u64; report::BLOCKS])],
        failure: i32,
    ) -> Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let ids: Vec<_> = records.iter().map(|(id, _, _)| *id).collect();
        let mut script = format!(
            "#!/bin/sh\nset -e\nprintf '%s %s\\n' \"$(basename \"$0\")\" \"$*\" >> \"$(dirname \"$0\")/calls.log\"\n[ {failure} -eq 0 ] || exit {failure}\nif [ \"$1\" = --list ]; then printf '%s\\n' {}; exit 0; fi\n[ \"$1\" = --output ] || exit 3\nout=$2\nshift 2\nwhile [ $# -gt 0 ]; do\n  case \"$2\" in\n",
            ids.join(" ")
        );
        for (id, instructions, cycles) in records {
            let record = json!({
                "id": id, "iters": 10, "instructions": vec![*instructions; report::BLOCKS],
                "cycles": cycles, "branch_misses": vec![0; report::BLOCKS],
                "branches": vec![0; report::BLOCKS], "cpus": vec![[0, 0]; report::BLOCKS],
                "retries": 0,
            });
            script.push_str(&format!("    {id}) echo '{record}' >> \"$out\" ;;\n"));
        }
        script.push_str("    *) exit 4 ;;\n  esac\n  shift 2\ndone\n");
        let path = dir.join(name);
        fs::write(&path, script)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    fn setup(
        dir: &Path,
        before: &[(&str, u64, [u64; report::BLOCKS])],
        after: &[(&str, u64, [u64; report::BLOCKS])],
    ) -> Result<(Vec<cases::Case>, Binaries, PathBuf)> {
        let a = fake_benchmark(dir, "before.sh", before, 0)?;
        let b = fake_benchmark(dir, "after.sh", after, 0)?;
        let listed = listing(&a, false)?;
        assert_eq!(listed, listing(&b, false)?);
        let cases = cases::group("reader", listed)?;
        let root = dir.join("run");
        fs::create_dir(&root)?;
        Ok((cases, BTreeMap::from([("reader".to_owned(), [a, b])]), root))
    }

    /// Output files of the fake benchmarks, in invocation order.
    fn calls(dir: &Path) -> Result<Vec<String>> {
        Ok(fs::read_to_string(dir.join("calls.log"))?
            .lines()
            .filter(|line| !line.contains("--list"))
            .map(|line| {
                let output = line.split_whitespace().nth(2).unwrap();
                Path::new(output)
                    .strip_prefix(dir.join("run"))
                    .unwrap()
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect())
    }

    #[test]
    fn measurements_interleave_sides_per_case_and_instructions_decide() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let steady = [2000; report::BLOCKS];
        let before = [
            ("reader/a/fold", 1000, steady),
            ("reader/a/reference", 500, [1000; report::BLOCKS]),
            ("reader/b/fold", 1000, steady),
            ("reader/b/reference", 500, [1000; report::BLOCKS]),
        ];
        let mut after = before;
        after[2].1 = 1020;
        let (cases, binaries, root) = setup(dir.path(), &before, &after)?;
        let measured = measure(&cases, &binaries, &root, 2, false)?;
        assert_eq!(
            calls(dir.path())?,
            [
                "reader-case-0001/before1",
                "reader-case-0001/after1",
                "reader-case-0001/before2",
                "reader-case-0001/after2",
                "reader-case-0002/before1",
                "reader-case-0002/after1",
                "reader-case-0002/before2",
                "reader-case-0002/after2",
            ]
        );
        assert!(root.join("reader-case-0002/after2.log").exists());
        let runs = &measured["reader"];
        assert_eq!(runs[0].len(), 2);
        assert_eq!(runs[1][1]["reader/b/fold"].process.instructions, 102.);
        let (status, text) = summarize(&measured)?;
        assert_eq!(status, report::Status::Fail);
        assert_eq!(status.exit_code(), 1);
        assert!(text.starts_with("\nreader\n"));
        assert!(text.contains("PASS reader/a/fold: instructions before=100.0 after=100.0"));
        assert!(
            text.contains(
                "FAIL reader/b/fold: instructions before=100.0 after=102.0 change=+2.00%"
            )
        );
        assert!(text.ends_with(
            "FAIL: 1 PASS, 1 FAIL (1 instructions, 0 cycles), 0 UNSTABLE library paths; 0 cycle warnings; 0 bistable\n"
        ));
        Ok(())
    }

    #[test]
    fn cycle_regressions_and_zero_readings_are_reported() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let before = [
            ("reader/a/fold", 1000, [6000; report::BLOCKS]),
            ("reader/a/reference", 500, [1000; report::BLOCKS]),
            ("reader/b/fold", 1000, [2000; report::BLOCKS]),
            ("reader/b/reference", 500, [1000; report::BLOCKS]),
        ];
        let mut after = before;
        after[0].2 = [6601; report::BLOCKS];
        after[2].2[4] = 0;
        let (cases, binaries, root) = setup(dir.path(), &before, &after)?;
        let (status, text) = summarize(&measure(&cases, &binaries, &root, 1, false)?)?;
        assert_eq!(status, report::Status::Fail);
        assert!(text.contains("FAIL reader/a/fold: instructions before=100.0 after=100.0 change=+0.00%; cycles min before=600.0 after=660.1 change=+10.02%"));
        assert!(text.contains(
            "  FAIL cycles: minimum +10.02% and median +10.02% on an operation of 600 cycles"
        ));
        assert!(text.contains("UNSTABLE reader/b/fold:"));
        assert!(text.contains("  UNSTABLE after: 1 zero counter readings after 0 repeats"));
        assert!(text.ends_with(
            "FAIL: 0 PASS, 1 FAIL (0 instructions, 1 cycles), 1 UNSTABLE library paths; 0 cycle warnings; 0 bistable\n"
        ));
        let mut only_zero = before;
        only_zero[2].2[4] = 0;
        let second = dir.path().join("second");
        fs::create_dir(&second)?;
        let (cases, binaries, root) = setup(&second, &before, &only_zero)?;
        let (status, _) = summarize(&measure(&cases, &binaries, &root, 1, false)?)?;
        assert_eq!(status, report::Status::Unstable);
        assert_eq!(status.exit_code(), 2);
        Ok(())
    }

    #[test]
    fn a_failing_benchmark_process_stops_the_run() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let records = [
            ("reader/a/fold", 1000, [2000; report::BLOCKS]),
            ("reader/a/reference", 500, [1000; report::BLOCKS]),
        ];
        let a = fake_benchmark(dir.path(), "before.sh", &records, 0)?;
        let b = fake_benchmark(dir.path(), "after.sh", &records, 5)?;
        let cases = cases::group("reader", listing(&a, false)?)?;
        let root = dir.path().join("run");
        fs::create_dir(&root)?;
        let binaries = BTreeMap::from([("reader".to_owned(), [a, b])]);
        let Err(error) = measure(&cases, &binaries, &root, 1, false) else {
            panic!("a failing process must stop the run")
        };
        assert!(
            format!("{error:#}").contains("benchmark failed: after1"),
            "{error:#}"
        );
        assert_eq!(
            calls(dir.path())?,
            ["reader-case-0001/before1", "reader-case-0001/after1"]
        );
        Ok(())
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
