//! The PostgreSQL build named by `PG_CONFIG` and a cluster of the tool's
//! own in it: a data directory per scale factor under
//! `target/bench-runs/`, with the Tessera modules preloaded.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{Scale, ServerSettings};

/// The contrib modules the tool needs: `pg_prewarm` reads the tables into
/// shared buffers before a timed run, `pg_buffercache` checks that they
/// stayed there.
pub const CONTRIB: [&str; 2] = ["pg_prewarm", "pg_buffercache"];

/// The database the data is loaded into.
pub const DATABASE: &str = "tpch";

/// A PostgreSQL build, as its `pg_config` describes it.
#[derive(Debug, Clone)]
pub struct Pg {
    pub pg_config: PathBuf,
    pub bindir: PathBuf,
    pub pkglibdir: PathBuf,
    pub sharedir: PathBuf,
    /// As `pg_config --version` prints it, such as `PostgreSQL 20devel`.
    pub version: String,
}

impl Pg {
    /// The build of `PG_CONFIG`, else of the `pg_config` found in `PATH`.
    pub fn discover() -> Result<Pg> {
        let pg_config: PathBuf = std::env::var_os("PG_CONFIG")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("pg_config"))
            .into();
        let output = Command::new(&pg_config)
            .args(["--bindir", "--pkglibdir", "--sharedir", "--version"])
            .output()
            .with_context(|| {
                format!(
                    "cannot run {}: set PG_CONFIG to the pg_config of a PostgreSQL build",
                    pg_config.display()
                )
            })?;
        if !output.status.success() {
            bail!(
                "{} failed: {}",
                pg_config.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let text = String::from_utf8(output.stdout)?;
        let lines: Vec<&str> = text.lines().collect();
        let [bindir, pkglibdir, sharedir, version] = lines[..] else {
            bail!("unexpected output of {}: {text}", pg_config.display());
        };
        let bindir = PathBuf::from(bindir);
        Ok(Pg {
            // The one in the build's own tree, so that make and the tool
            // name the same build whatever PATH holds.
            pg_config: bindir.join("pg_config"),
            bindir,
            pkglibdir: pkglibdir.into(),
            sharedir: sharedir.into(),
            version: version.to_string(),
        })
    }

    /// The major version, such as `20`, as a data directory's `PG_VERSION`
    /// records it.
    pub fn major(&self) -> &str {
        let number = self.version.rsplit(' ').next().unwrap_or_default();
        let end = number
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(number.len());
        &number[..end]
    }

    pub fn bin(&self, program: &str) -> PathBuf {
        self.bindir.join(program)
    }

    /// The suffix of loadable modules on this platform.
    pub fn dlsuffix() -> &'static str {
        if cfg!(target_os = "macos") {
            ".dylib"
        } else {
            ".so"
        }
    }

    /// The installed files of Tessera and the server, whose hashes say
    /// which build a run measured.
    pub fn binaries(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = ["tessera", "tessera_nodes", "tessera_kernels"]
            .iter()
            .map(|name| self.pkglibdir.join(format!("{name}{}", Pg::dlsuffix())))
            .collect();
        files.push(self.pkglibdir.join("libtessera_runtime.a"));
        files.push(self.bin("postgres"));
        files
    }

    /// Fails with the commands that install the contrib modules the build
    /// lacks.
    pub fn require_contrib(&self) -> Result<()> {
        let missing: Vec<&str> = CONTRIB
            .into_iter()
            .filter(|name| {
                !self
                    .sharedir
                    .join("extension")
                    .join(format!("{name}.control"))
                    .exists()
            })
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        // A build installed into its own tree keeps the contrib makefiles
        // next to bin/; otherwise the source tree is the user's to name.
        let tree = self.bindir.parent().map(Path::to_path_buf);
        let contrib = tree
            .filter(|tree| {
                tree.join("contrib")
                    .join(missing[0])
                    .join("Makefile")
                    .exists()
            })
            .map(|tree| tree.join("contrib"))
            .unwrap_or_else(|| PathBuf::from("<PostgreSQL build tree>/contrib"));
        let commands: Vec<String> = missing
            .iter()
            .map(|name| format!("make -C {}/{name} install", contrib.display()))
            .collect();
        bail!(
            "{} not installed in the PostgreSQL build of {}; install with:\n  {}",
            missing.join(" and "),
            self.pg_config.display(),
            commands.join("\n  ")
        )
    }
}

/// Builds the release libraries of Tessera and installs them into the
/// build of `pg`, as the README asks before any timing: a debug build left
/// installed by a test run would be measured otherwise.
pub fn install_tessera(root: &Path, pg: &Pg, log: &Path) -> Result<()> {
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let pg_config = format!("PG_CONFIG={}", pg.pg_config.display());
    let mut file = File::create(log)?;
    for target in [None, Some("install")] {
        let mut make = Command::new("make");
        make.arg("-C")
            .arg(root)
            .arg(format!("-j{jobs}"))
            .arg(&pg_config)
            .arg("RUST_PROFILE=release");
        make.args(target);
        writeln!(file, "$ {make:?}")?;
        let status = make
            .stdout(file.try_clone()?)
            .stderr(file.try_clone()?)
            .status()
            .context("cannot run make")?;
        if !status.success() {
            bail!("{make:?} failed:\n{}", tail(log, 30));
        }
    }
    Ok(())
}

/// The last lines of a log, for an error message.
pub fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The cluster of one scale factor.
pub struct Cluster {
    pub pg: Pg,
    pub data: PathBuf,
    pub log: PathBuf,
    pub settings: ServerSettings,
}

impl Cluster {
    pub fn new(pg: Pg, runs: &Path, scale: &Scale, settings: ServerSettings) -> Cluster {
        Cluster {
            pg,
            data: runs.join(format!("pgdata-tpch-sf{scale}")),
            log: runs.join(format!("tpch-sf{scale}-server.log")),
            settings,
        }
    }

    fn exists(&self) -> bool {
        self.data.join("PG_VERSION").exists()
    }

    /// Whether the server of this data directory runs.
    pub fn running(&self) -> Result<bool> {
        if !self.exists() {
            return Ok(false);
        }
        let status = Command::new(self.pg.bin("pg_ctl"))
            .arg("-D")
            .arg(&self.data)
            .arg("status")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }

    /// Creates the cluster if there is none, writes its settings and
    /// starts it, or restarts it when it runs: the postmaster holds the
    /// modules it loaded, and every backend is forked from it, so a run
    /// measures the modules installed last only after a restart. Returns
    /// whether the server ran before.
    pub fn start(&self) -> Result<bool> {
        if !self.exists() {
            self.initdb()?;
        }
        let version = fs::read_to_string(self.data.join("PG_VERSION"))?;
        if version.trim() != self.pg.major() {
            bail!(
                "{} holds a cluster of PostgreSQL {}, and {} is {}; remove the directory to start anew",
                self.data.display(),
                version.trim(),
                self.pg.pg_config.display(),
                self.pg.version
            );
        }
        fs::write(self.data.join("tessera-tpch.conf"), self.settings.conf())?;
        let running = self.running()?;
        if !running {
            self.check_port()?;
        }
        let action = if running { "restart" } else { "start" };
        let status = Command::new(self.pg.bin("pg_ctl"))
            .arg("-D")
            .arg(&self.data)
            .arg("-l")
            .arg(&self.log)
            .args(["-m", "fast", "-w", action])
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("pg_ctl {action} failed:\n{}", tail(&self.log, 20));
        }
        self.create_database()?;
        Ok(running)
    }

    /// Stops the server if it runs; the data stays.
    pub fn stop(&self) -> Result<bool> {
        if !self.running()? {
            return Ok(false);
        }
        let status = Command::new(self.pg.bin("pg_ctl"))
            .arg("-D")
            .arg(&self.data)
            .args(["-m", "fast", "-w", "stop"])
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("pg_ctl stop failed:\n{}", tail(&self.log, 20));
        }
        Ok(true)
    }

    fn initdb(&self) -> Result<()> {
        fs::create_dir_all(self.data.parent().unwrap())?;
        let log = self.data.with_extension("initdb.log");
        // The C locale: the answers of TPC-H sort strings by their bytes,
        // and both modes compare them under one collation.
        let status = Command::new(self.pg.bin("initdb"))
            .arg("-D")
            .arg(&self.data)
            .args(["-A", "trust", "-E", "UTF8", "--no-locale"])
            .stdout(File::create(&log)?)
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            bail!("initdb failed:\n{}", tail(&log, 20));
        }
        let mut conf = OpenOptions::new()
            .append(true)
            .open(self.data.join("postgresql.conf"))?;
        writeln!(conf, "\ninclude_if_exists = 'tessera-tpch.conf'")?;
        Ok(())
    }

    /// Fails with the data directory of another server that holds the
    /// port, which a socket's lock file names on its second line.
    fn check_port(&self) -> Result<()> {
        let lock = Path::new(&self.settings.socket_dir)
            .join(format!(".s.PGSQL.{}.lock", self.settings.port));
        let Ok(text) = fs::read_to_string(&lock) else {
            return Ok(());
        };
        let owner = text.lines().nth(1).unwrap_or("another server");
        bail!(
            "port {} is taken by {owner}; stop that server (cargo tpch stop --sf <its scale>) or pass --port",
            self.settings.port
        )
    }

    /// A connection to `dbname` as the operating system's user, whom
    /// initdb made the superuser.
    pub fn connect_to(&self, dbname: &str) -> Result<postgres::Client> {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".into());
        postgres::Config::new()
            .host(&self.settings.socket_dir)
            .port(self.settings.port)
            .user(&user)
            .dbname(dbname)
            .application_name("tessera-tpch")
            .connect(postgres::NoTls)
            .with_context(|| format!("cannot connect to {dbname} on port {}", self.settings.port))
    }

    pub fn connect(&self) -> Result<postgres::Client> {
        self.connect_to(DATABASE)
    }

    fn create_database(&self) -> Result<()> {
        let mut admin = self.connect_to("postgres")?;
        let exists = admin
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&DATABASE])?
            .is_some();
        if !exists {
            admin.batch_execute(&format!("CREATE DATABASE {DATABASE}"))?;
        }
        let mut client = self.connect()?;
        client.batch_execute(
            "CREATE EXTENSION IF NOT EXISTS tessera;
             CREATE EXTENSION IF NOT EXISTS pg_prewarm;
             CREATE EXTENSION IF NOT EXISTS pg_buffercache;",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(version: &str) -> Pg {
        Pg {
            pg_config: "/pg/bin/pg_config".into(),
            bindir: "/pg/bin".into(),
            pkglibdir: "/pg/lib".into(),
            sharedir: "/pg/share".into(),
            version: version.into(),
        }
    }

    #[test]
    fn major_versions() {
        assert_eq!(pg("PostgreSQL 20devel").major(), "20");
        assert_eq!(pg("PostgreSQL 19.1").major(), "19");
        assert_eq!(pg("PostgreSQL 19beta2").major(), "19");
    }

    #[test]
    fn binaries_name_the_modules_and_the_server() {
        let files = pg("PostgreSQL 20devel").binaries();
        let suffix = Pg::dlsuffix();
        assert!(files.contains(&PathBuf::from(format!("/pg/lib/tessera_nodes{suffix}"))));
        assert!(files.contains(&PathBuf::from("/pg/bin/postgres")));
    }

    #[test]
    fn clusters_are_per_scale() {
        let settings = ServerSettings {
            port: 5434,
            socket_dir: "/tmp".into(),
            shared_buffers: "2GB".into(),
        };
        let scale = Scale::parse("0.01").unwrap();
        let cluster = Cluster::new(
            pg("PostgreSQL 20devel"),
            Path::new("/runs"),
            &scale,
            settings,
        );
        assert_eq!(cluster.data, Path::new("/runs/pgdata-tpch-sf0.01"));
        assert_eq!(cluster.log, Path::new("/runs/tpch-sf0.01-server.log"));
    }
}
