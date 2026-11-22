//! The directory of a run, target/bench-runs/tpch-sf<N>-<id>/, as every
//! measurement of the repository keeps one: what was run on which build
//! and machine (source.txt, pg_settings.txt), and what came out
//! (results.txt and the files the later steps add, run.json for the
//! report).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
pub use tessera_pgtool::run::{head, power};
use tessera_pgtool::run::{identifier, load_average, output, sha256};

use crate::check::QueryCheck;
use crate::cluster::Cluster;
use crate::config::Scale;
use crate::measure::{Failure, Sample};
use crate::participation::QueryParticipation;

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
    /// The timing of `run`: none for `check`.
    #[serde(default)]
    pub pairs: u32,
    #[serde(default)]
    pub warmups: u32,
    /// Percent within which a ratio counts as even.
    #[serde(default)]
    pub threshold: f64,
}

/// What run.json holds: everything the report prints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub meta: Meta,
    pub checks: Vec<QueryCheck>,
    #[serde(default)]
    pub participation: Vec<QueryParticipation>,
    /// The timed executions of `run`.
    #[serde(default)]
    pub samples: Vec<Sample>,
    #[serde(default)]
    pub failures: Vec<Failure>,
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
    let _ = writeln!(text, "load average {}", load_average(root));
    let _ = writeln!(text, "sizes:");
    for (table, size) in sizes {
        let _ = writeln!(text, "  {table} {size}");
    }
    text
}
