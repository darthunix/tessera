//! Compare compatible source snapshots with Criterion, in A/B/B/A order per case.
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
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

#[derive(Parser, Debug)]
#[command(
    about = "Compare compatible Rust revisions with Criterion (before/after/after/before per case).",
    after_help = "REF is a Git revision or WORKTREE. Defaults to both full benchmarks.\nFilters must retain each selected case's reference and a library path.\nExit: 0 PASS, 1 regression, 2 UNSTABLE or invalid/incomplete run.\nRun on an idle machine; results are never retried or overwritten."
)]
struct Options {
    #[arg(long, value_name = "REF")]
    base: String,
    #[arg(long, value_name = "REF", default_value = "WORKTREE")]
    candidate: String,
    #[arg(long, value_parser = ["column_reader", "filter_int32"])]
    bench: Option<String>,
    #[arg(long, value_name = "REGEX")]
    filter: Option<String>,
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

fn benchmark(executable: &Path, directory: &Path, filter: Option<&str>) -> Command {
    let mut cmd = Command::new(executable);
    cmd.arg("--bench")
        .args(["--noplot", "--color", "never"])
        .env("CRITERION_HOME", directory)
        .env_remove("CARGO_CRITERION_PORT");
    if let Some(filter) = filter {
        cmd.arg("--").arg(filter);
    }
    cmd
}

fn listing(executable: &Path, directory: &Path, filter: Option<&str>) -> Result<BTreeSet<String>> {
    // Insert --list before the optional positional regex separator.
    let mut cmd = benchmark(executable, directory, None);
    cmd.arg("--list");
    if let Some(filter) = filter {
        cmd.arg("--").arg(filter);
    }
    report::listed(&text(&mut cmd)?)
}

fn collect(
    executable: &Path,
    root: &Path,
    name: &str,
    expected: &BTreeSet<String>,
    filter: Option<&str>,
) -> Result<report::Run> {
    let directory = root.join(name);
    fs::create_dir(&directory)?;
    let log = create(&root.join(format!("{name}.log")))?;
    println!(
        "Measuring {name}: {} paths; log {}",
        expected.len(),
        root.join(format!("{name}.log")).display()
    );
    let status = benchmark(executable, &directory, filter)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .status()?;
    ensure!(status.success(), "benchmark failed: {name}");
    report::load(&directory, expected).with_context(|| format!("invalid measurement: {name}"))
}

fn compare(repo: &Path, root: &Path, options: &Options) -> Result<u8> {
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
        "mode":if options.filter.is_some() {"diagnostic"} else {"full"},
        "filter":options.filter,"order_unit":"case","order":cases::ORDER.map(|(name, _)| name),
        "measurement":{"samples":100,"warm_up_ms":100,"measurement_ms":1000,"confidence_level":0.99,"noise_threshold":0.03}}),
    )?;
    let benches: Vec<_> = options.bench.as_deref().map_or_else(
        || vec!["column_reader", "filter_int32"],
        |bench| vec![bench],
    );
    // Finish every build and listing before starting any timed process.
    let mut binaries = Vec::new();
    let mut identities = BTreeMap::new();
    for bench in benches {
        let a = build(&before, bench, root, "before")?;
        let b = build(&after, bench, root, "after")?;
        let expected = listing(&a, &root.join("listing-before"), options.filter.as_deref())?;
        ensure!(
            expected == listing(&b, &root.join("listing-after"), options.filter.as_deref())?,
            "benchmark case sets differ"
        );
        for (side, binary) in [("before", &a), ("after", &b)] {
            identities.insert(
                format!("{side}/{bench}"),
                json!({"path":binary,"sha256":snapshot::digest(&fs::read(binary)?)}),
            );
        }
        let cases = cases::group(bench, expected)?;
        save(&root.join(format!("{bench}-cases.json")), &cases)?;
        for case in &cases {
            fs::create_dir(root.join(&case.directory))?;
        }
        binaries.push((a, b, cases));
    }
    save(&root.join("binaries.json"), &identities)?;
    let mut report = create(&root.join("report.txt"))?;
    writeln!(
        report,
        "{}; baseline={}, candidate={}",
        if options.filter.is_some() {
            "DIAGNOSTIC: selected cases only"
        } else {
            "FULL RUN"
        },
        before.revision,
        after.revision
    )?;
    let mut outcome = report::Status::Pass;
    for (a, b, cases) in binaries {
        let runs = cases::measure(&cases, |case, name, side| {
            println!("Case {}: {}", case.directory, case.name);
            collect(
                [&a, &b][side],
                &root.join(&case.directory),
                name,
                &case.paths,
                Some(&case.filter()),
            )
        })?;
        let mut section = Vec::new();
        let status = report::print(&mut section, &runs)?;
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
        "{}; no retries, no discarded measurements. Run on an idle machine.",
        if options.filter.is_some() {
            "DIAGNOSTIC RUN"
        } else {
            "FULL RUN"
        }
    );
    let result = compare(&repo, &root, &options);
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
        assert!(
            Options::try_parse_from(["tessera-bench", "--base", "HEAD", "--bench", "unknown"])
                .is_err()
        );
        assert!(Options::try_parse_from(["tessera-bench", "--base", "HEAD", "--quick"]).is_err());
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

    #[test]
    #[ignore = "set TESSERA_BENCH_EXECUTABLE to a built Criterion benchmark"]
    fn criterion_case_filters_match_listing() -> Result<()> {
        let binary = PathBuf::from(std::env::var("TESSERA_BENCH_EXECUTABLE")?);
        let dir = tempfile::tempdir()?;
        let all = listing(&binary, dir.path(), None)?;
        let subset = all
            .iter()
            .filter(|id| {
                id.rsplit_once('/')
                    .is_some_and(|(_, path)| matches!(path, "fold" | "scalar" | "reference"))
            })
            .cloned()
            .collect();
        for selected in [all, subset] {
            for case in cases::group("test", selected)? {
                assert_eq!(
                    listing(&binary, dir.path(), Some(&case.filter()))?,
                    case.paths,
                    "{}",
                    case.name
                );
            }
        }
        Ok(())
    }
}
