//! The directory of a run, target/bench-runs/tpch-sf<N>-<id>/, as every
//! measurement of the repository keeps one: what was run on which build
//! and machine (source.txt, pg_settings.txt), and what came out
//! (results.txt and the files the later steps add, run.json for the
//! report).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::check::QueryCheck;
use crate::cluster::Cluster;
use crate::config::Scale;

/// What a run was: enough to read its results without its directory's
/// other files, and to refuse comparing runs that differ in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// `check` or `run`.
    pub command: String,
    pub sf: String,
    pub schema: String,
    pub workers: u32,
    pub work_mem: String,
    pub jit: bool,
    pub timeout_s: u64,
    /// The commit of the repository, with `+` when the tree had changes.
    pub head: String,
    pub postgres: String,
    /// The server's clock at the start.
    pub started: String,
    /// The power source, as pmset reports it on macOS.
    pub power: String,
}

/// What run.json holds: everything the report prints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub meta: Meta,
    pub checks: Vec<QueryCheck>,
}

pub struct RunDir {
    pub path: PathBuf,
    pub id: String,
}

impl RunDir {
    /// A new directory under `runs` with an identifier of its own.
    pub fn create(runs: &Path, scale: &Scale) -> Result<RunDir> {
        loop {
            let id = identifier();
            let path = runs.join(format!("tpch-sf{scale}-{id}"));
            if !path.exists() {
                fs::create_dir_all(&path)?;
                return Ok(RunDir { path, id });
            }
        }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn write(&self, name: &str, text: &str) -> Result<()> {
        let path = self.file(name);
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
    }
}

/// Six letters and digits, as the run directories of bench/pg have.
fn identifier() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos() as u64);
    let mut state = nanos ^ u64::from(std::process::id()).rotate_left(32);
    (0..6)
        .map(|_| {
            state = splitmix(&mut state);
            ALPHABET[(state % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

/// One step of SplitMix64.
pub fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The output of a command, empty when it fails or is missing.
fn output(program: &str, args: &[&str], dir: &Path) -> String {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string()
        })
        .unwrap_or_default()
}

/// The commit, marked `+` when tracked files changed.
pub fn head(root: &Path) -> String {
    let commit = output("git", &["rev-parse", "--short=12", "HEAD"], root);
    let changed = !output("git", &["status", "--short", "--untracked-files=no"], root).is_empty();
    format!("{commit}{}", if changed { "+" } else { "" })
}

/// The power source, which changes the speed of a laptop.
pub fn power(root: &Path) -> String {
    let text = output("pmset", &["-g", "batt"], root);
    text.lines()
        .next()
        .and_then(|line| line.split('\'').nth(1))
        .unwrap_or("unknown")
        .to_string()
}

fn sha256(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => Sha256::digest(&bytes)
            .iter()
            .fold(String::new(), |mut text, byte| {
                let _ = write!(text, "{byte:02x}");
                text
            }),
        Err(_) => "missing".into(),
    }
}

/// source.txt: the commit and the state of the tree, the installed
/// libraries' hashes, the machine's power and load, the table sizes.
pub fn source(root: &Path, cluster: &Cluster, meta: &Meta, sizes: &[(String, i64)]) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "HEAD {}", output("git", &["rev-parse", "HEAD"], root));
    let _ = writeln!(text, "status:");
    for line in output("git", &["status", "--short", "--untracked-files=no"], root).lines() {
        let _ = writeln!(text, "  {line}");
    }
    let _ = writeln!(
        text,
        "command {}\nsf {}\nschema {}\nworkers {}\nwork_mem {}\njit {}\ntimeout {} s",
        meta.command,
        meta.sf,
        meta.schema,
        meta.workers,
        meta.work_mem,
        if meta.jit { "on" } else { "off" },
        meta.timeout_s
    );
    let _ = writeln!(text, "pg_config {}", cluster.pg.pg_config.display());
    let _ = writeln!(text, "postgres {}", meta.postgres);
    let _ = writeln!(text, "shared_buffers {}", cluster.settings.shared_buffers);
    let _ = writeln!(text, "sha256:");
    for file in cluster.pg.binaries() {
        let _ = writeln!(text, "  {}  {}", sha256(&file), file.display());
    }
    let _ = writeln!(text, "started {}", meta.started);
    let _ = writeln!(text, "power {}", meta.power);
    let load = if cfg!(target_os = "macos") {
        output("sysctl", &["-n", "vm.loadavg"], root)
    } else {
        fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let _ = writeln!(text, "load average {load}");
    let _ = writeln!(text, "sizes:");
    for (table, size) in sizes {
        let _ = writeln!(text, "  {table} {size}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_six_letters_or_digits() {
        let first = identifier();
        assert_eq!(first.len(), 6);
        assert!(first.bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn splitmix_is_the_reference_sequence() {
        // The first outputs of SplitMix64 seeded with 1234567.
        let mut state = 1_234_567;
        assert_eq!(splitmix(&mut state), 6_457_827_717_110_365_317);
        assert_eq!(splitmix(&mut state), 3_203_168_211_198_807_973);
    }

    #[test]
    fn hashes_are_hex() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            sha256(&file),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256(&dir.path().join("none")), "missing");
    }
}
