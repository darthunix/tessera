//! A PostgreSQL build and a cluster of a tool's own in it, with the
//! Tessera modules preloaded: what the tools that run SQL against Tessera
//! share (tessera-tpch, tessera-crosscheck). The build is the one
//! `PG_CONFIG` names; the cluster is a data directory under the tool's run
//! directory, created, configured, started and stopped by the tool. The
//! tools that compare two revisions (tessera-bench, tessera-ab) share the
//! snapshots of their sources ([`snapshot`]).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

pub mod snapshot;

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

    /// Fails with the commands that install the contrib modules among
    /// `names` the build lacks.
    pub fn require_contrib(&self, names: &[&str]) -> Result<()> {
        let missing: Vec<&str> = names
            .iter()
            .copied()
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
    install_tessera_with(root, pg, log, &[], None)
}

/// [`install_tessera`] with variables of make for both its runs, such as
/// `COPT=-falign-functions=64`: a comparison of two revisions builds both
/// with the same ones. Past `deadline` make is stopped, as [`run_until`]
/// stops a step, and the call fails.
pub fn install_tessera_with(
    root: &Path,
    pg: &Pg,
    log: &Path,
    variables: &[String],
    deadline: Option<Instant>,
) -> Result<()> {
    let jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let pg_config = format!("PG_CONFIG={}", pg.pg_config.display());
    let mut file = File::create(log)?;
    for target in [None, Some("install")] {
        let mut make = Command::new("make");
        // Run from an outer make (`make tpch`), its flags and jobserver
        // would reach this one and conflict with its -j.
        make.env_remove("MAKEFLAGS")
            .env_remove("MFLAGS")
            .env_remove("MAKELEVEL")
            .arg("-C")
            .arg(root)
            .arg(format!("-j{jobs}"))
            .arg(&pg_config)
            .arg("RUST_PROFILE=release")
            .args(variables);
        make.args(target);
        writeln!(file, "$ {make:?}")?;
        make.stdout(file.try_clone()?).stderr(file.try_clone()?);
        let status = run_until(&mut make, deadline).context("cannot run make")?;
        if !status.success() {
            bail!("{make:?} failed:\n{}", tail(log, 30));
        }
    }
    Ok(())
}

/// Run a step of a tool to its end, or stop it at `deadline`.
///
/// With a deadline the step runs in a process group of its own, so that
/// the deadline ends it with every process it started, such as the `psql`
/// of a script: the group gets SIGTERM, a second to go, then SIGKILL, so
/// that a process that ignores SIGTERM ends too. A process that leaves the
/// group itself, as `pg_ctl` starts a server in a session of its own,
/// outlives it. A step that ends after the deadline fails as one still
/// running would. Ctrl-C at the terminal does not reach the group; the
/// group's leader, a watchdog started before the step, ends it in the
/// same way when the tool ends first, by Ctrl-C or any signal. The step's
/// output goes where the command sends it. Without a deadline the call
/// waits as [`Command::status`] does, the step in the tool's own process
/// group. With one the command is left set to join a group that is gone
/// after the call: a command serves one step.
///
/// # Errors
///
/// The deadline passed before the step started, while it ran, or before
/// its end was seen; or the step or its watchdog could not start; or the
/// group could not be signalled, or outlived SIGKILL, so that some of it
/// may be left.
pub fn run_until(command: &mut Command, deadline: Option<Instant>) -> Result<ExitStatus> {
    let Some(deadline) = deadline else {
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {command:?}"))?;
        return Ok(child.wait()?);
    };
    ensure!(
        Instant::now() < deadline,
        "the deadline passed before {command:?}"
    );
    // Unreleased on an error below, the watchdog ends its group.
    let watchdog = Watchdog::start()?;
    let mut child = command
        .process_group(watchdog.group())
        .spawn()
        .with_context(|| format!("cannot start {command:?}"))?;
    loop {
        let status = child.try_wait()?;
        let now = Instant::now();
        if now >= deadline {
            // The step may have ended and left processes of its group.
            let ended = watchdog.end_group();
            // The step's own id is still its own: it is not reaped yet.
            let _ = child.kill();
            child.wait()?;
            ended?;
            bail!(
                "the deadline passed before the end of {command:?} was seen; its group was stopped"
            );
        }
        if let Some(status) = status {
            watchdog.release();
            return Ok(status);
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

/// The leader of a step's process group: a shell started before the step,
/// which joins its group, so that the step never runs unwatched. It waits
/// for a line on a pipe only the tool holds. The line releases it; the
/// pipe's end, which comes with the tool's end however it ends, or with
/// [`Watchdog::end_group`], makes it signal its own group: SIGTERM, which
/// it ignores itself, a second, then SIGKILL to every member, itself last.
/// The group's number is the watchdog's process id, which no other process
/// or group can take before the watchdog is reaped: no signal of it, nor
/// of the tool, reaches a group that took the number since.
struct Watchdog(std::process::Child);

/// Reads a line, or else ends its own group.
const WATCH: &str =
    "trap '' TERM; read -r _ && exit 0; kill -s TERM 0 && sleep 1 && kill -s KILL 0; exit 3";

impl Watchdog {
    fn start() -> Result<Self> {
        let child = Command::new("/bin/sh")
            .args(["-c", WATCH])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot start the watchdog of a step")?;
        Ok(Self(child))
    }

    /// The step's process group, the watchdog's own.
    fn group(&self) -> i32 {
        // A process id fits an i32, as pid_t.
        self.0.id().cast_signed()
    }

    /// The step's end is seen: the watchdog leaves its group alone.
    fn release(mut self) {
        if let Some(mut pipe) = self.0.stdin.take() {
            // A watchdog that is gone already has nothing to release.
            let _ = pipe.write_all(b"\n");
        }
    }

    /// End the step's group, and see every member of it end, a zombie at
    /// most, before the watchdog, which holds the group's number, is
    /// reaped. Its end by its own SIGKILL shows that both signals went out.
    fn end_group(mut self) -> Result<()> {
        let group = self.group();
        drop(self.0.stdin.take());
        let gone = Instant::now() + Duration::from_secs(6);
        while live_member(&processes()?, group) {
            ensure!(Instant::now() < gone, "the group {group} outlived SIGKILL");
            thread::sleep(Duration::from_millis(50));
        }
        let status = self
            .0
            .wait()
            .context("cannot wait for the watchdog of a step")?;
        ensure!(
            status.signal() == Some(SIGKILL),
            "the watchdog of the group {group} could not signal it: {status}"
        );
        Ok(())
    }
}

impl Drop for Watchdog {
    /// Reaped whatever happened; unreleased, it ends its group first.
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

/// SIGKILL, the same number on every Unix.
const SIGKILL: i32 = 9;

/// The process group and the state of every process, as `ps -A -o pgid=
/// -o stat=` lists them.
fn processes() -> Result<String> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pgid=", "-o", "stat="])
        .stderr(Stdio::null())
        .output()
        .context("cannot run ps")?;
    ensure!(output.status.success(), "ps failed");
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a listing of [`processes`] has a member of `group` that is not
/// a zombie.
fn live_member(listing: &str, group: i32) -> bool {
    listing.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next().and_then(|id| id.parse().ok()) == Some(group)
            && fields.next().is_some_and(|state| !state.starts_with('Z'))
    })
}

/// The last lines of a log, for an error message.
pub fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// A command that starts a server, run through `sh` closing descriptors
/// 3 to 9 first. The postmaster outlives the tool and keeps the
/// descriptors it inherits; run from `make tpch`, it kept make's own pipe
/// open, and make waited for its end forever. The standard library marks
/// its own descriptors close-on-exec, but cannot close inherited ones
/// without unsafe code, and a POSIX shell (dash on Linux) redirects only
/// descriptors 0 to 9, among which make's are.
fn daemon(program: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(CLOSE_AND_EXEC).arg(program);
    command
}

/// Closes descriptors 3 to 9 and runs `$0` with the arguments.
const CLOSE_AND_EXEC: &str = r#"exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-; exec "$0" "$@""#;

/// What a tool's cluster is: its name, database, settings and modules.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSpec {
    /// The data directory is `pgdata-<name>` and the server's log
    /// `<name>-server.log`, both in the tool's run directory.
    pub name: String,
    /// The database the tool works in, created at the first start.
    pub database: String,
    /// The application name of the tool's connections.
    pub application: String,
    /// The settings file `postgresql.conf` includes, rewritten at every
    /// start, and its text: the settings that need a restart.
    pub conf_name: String,
    pub conf: String,
    pub port: u16,
    /// The directory of the server's socket.
    pub socket_dir: String,
    /// The arguments initdb takes besides the directory, trust and UTF8,
    /// such as `--no-locale`.
    pub initdb_args: Vec<String>,
    /// The extensions created in the database at every start.
    pub extensions: Vec<String>,
    /// How a user stops the server of another cluster that holds the
    /// port, for the error that says so.
    pub stop_hint: String,
}

/// A cluster of a tool's own.
pub struct Cluster {
    pub pg: Pg,
    pub data: PathBuf,
    pub log: PathBuf,
    pub spec: ClusterSpec,
}

impl Cluster {
    pub fn new(pg: Pg, runs: &Path, spec: ClusterSpec) -> Cluster {
        Cluster {
            pg,
            data: runs.join(format!("pgdata-{}", spec.name)),
            log: runs.join(format!("{}-server.log", spec.name)),
            spec,
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
        fs::write(self.data.join(&self.spec.conf_name), &self.spec.conf)?;
        let running = self.running()?;
        if !running {
            self.check_port()?;
        }
        let action = if running { "restart" } else { "start" };
        let status = daemon(&self.pg.bin("pg_ctl"))
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
        fs::create_dir_all(
            self.data
                .parent()
                .context("a data directory without a parent")?,
        )?;
        let log = self.data.with_extension("initdb.log");
        let status = Command::new(self.pg.bin("initdb"))
            .arg("-D")
            .arg(&self.data)
            .args(["-A", "trust", "-E", "UTF8"])
            .args(&self.spec.initdb_args)
            .stdout(File::create(&log)?)
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            bail!("initdb failed:\n{}", tail(&log, 20));
        }
        let mut conf = OpenOptions::new()
            .append(true)
            .open(self.data.join("postgresql.conf"))?;
        writeln!(conf, "\ninclude_if_exists = '{}'", self.spec.conf_name)?;
        Ok(())
    }

    /// Fails with the data directory of another server that holds the
    /// port, which a socket's lock file names on its second line.
    fn check_port(&self) -> Result<()> {
        let lock =
            Path::new(&self.spec.socket_dir).join(format!(".s.PGSQL.{}.lock", self.spec.port));
        let Ok(text) = fs::read_to_string(&lock) else {
            return Ok(());
        };
        let owner = text.lines().nth(1).unwrap_or("another server");
        bail!(
            "port {} is taken by {owner}; stop that server ({}) or pass --port",
            self.spec.port,
            self.spec.stop_hint
        )
    }

    /// A connection to `dbname` as the operating system's user, whom
    /// initdb made the superuser.
    pub fn connect_to(&self, dbname: &str) -> Result<postgres::Client> {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".into());
        postgres::Config::new()
            .host(&self.spec.socket_dir)
            .port(self.spec.port)
            .user(&user)
            .dbname(dbname)
            .application_name(&self.spec.application)
            .connect(postgres::NoTls)
            .with_context(|| format!("cannot connect to {dbname} on port {}", self.spec.port))
    }

    /// A connection to the tool's database.
    pub fn connect(&self) -> Result<postgres::Client> {
        self.connect_to(&self.spec.database)
    }

    fn create_database(&self) -> Result<()> {
        let mut admin = self.connect_to("postgres")?;
        let database = &self.spec.database;
        let exists = admin
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[database])?
            .is_some();
        if !exists {
            admin.batch_execute(&format!("CREATE DATABASE {database}"))?;
        }
        let mut client = self.connect()?;
        for extension in &self.spec.extensions {
            client.batch_execute(&format!("CREATE EXTENSION IF NOT EXISTS {extension}"))?;
        }
        Ok(())
    }
}

/// An error as `SQLSTATE: message`, or the client's own description.
pub fn message(error: &postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("{}: {}", db.code().code(), db.message()),
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_within_its_deadline_gives_its_status() -> Result<()> {
        let step = || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 3"]);
            command
        };
        let status = run_until(&mut step(), Some(Instant::now() + Duration::from_secs(30)))?;
        assert_eq!(status.code(), Some(3));
        let status = run_until(&mut step(), None)?;
        assert_eq!(status.code(), Some(3));
        Ok(())
    }

    /// Whether a process of the given id still runs, after its parent is
    /// gone and it is reaped.
    fn alive(pid: &str) -> Result<bool> {
        for _ in 0..100 {
            let running = Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()?
                .success();
            if !running {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(true)
    }

    #[test]
    fn a_step_that_ignores_sigterm_ends_at_the_deadline() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pid = directory.path().join("pid");
        // The leader ignores SIGTERM, and so does a descendant it leaves.
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "trap '' TERM; (trap '' TERM; exec sleep 60) & echo $! > '{}'; sleep 60",
            pid.display()
        ));
        let started = Instant::now();
        let error = run_until(&mut command, Some(started + Duration::from_millis(300)));
        assert!(error.is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            !alive(&fs::read_to_string(&pid)?)?,
            "a descendant outlived the deadline"
        );
        Ok(())
    }

    #[test]
    fn a_step_that_ended_leaves_no_process_past_the_deadline() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pid = directory.path().join("pid");
        // The step ends at its deadline, whether just before the poll that
        // sees the deadline or just after, and leaves a descendant that
        // ignores SIGTERM: the deadline ends the descendant either way.
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "(trap '' TERM; exec sleep 60) & echo $! > '{}'; sleep 0.3; exit 0",
            pid.display()
        ));
        let started = Instant::now();
        let outcome = run_until(&mut command, Some(started + Duration::from_millis(300)));
        assert!(outcome.is_err());
        assert!(
            !alive(&fs::read_to_string(&pid)?)?,
            "a descendant outlived the deadline"
        );
        Ok(())
    }

    #[test]
    fn a_watchdog_dropped_unreleased_ends_its_group() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pid = directory.path().join("pid");
        let watchdog = Watchdog::start()?;
        let mut step = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "/bin/sh -c \"trap '' TERM; echo \\$\\$ > '{}'; exec sleep 60\" & wait",
                pid.display()
            ))
            .process_group(watchdog.group())
            .spawn()?;
        while fs::read_to_string(&pid).map_or(true, |text| text.trim().is_empty()) {
            thread::sleep(Duration::from_millis(20));
        }
        // As on an error of the tool: the drop ends the group, long before
        // the sleep would end by itself.
        let dropped = Instant::now();
        drop(watchdog);
        while step.try_wait()?.is_none() {
            assert!(
                dropped.elapsed() < Duration::from_secs(10),
                "the step outlived its watchdog"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !alive(&fs::read_to_string(&pid)?)?,
            "a member outlived its watchdog"
        );
        Ok(())
    }

    #[test]
    fn a_group_of_zombies_alone_is_ended() {
        let listing = "  1 Ss\n 123 Z\n 123 Z+\n1234 S\n";
        assert!(!live_member(listing, 123), "zombies are ended");
        assert!(live_member(&format!("{listing} 123 R+\n"), 123));
        assert!(!live_member(listing, 12), "a group by its whole id");
    }

    #[test]
    fn a_step_that_ends_after_the_deadline_fails() -> Result<()> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 0.05; exit 0"]);
        let outcome = run_until(
            &mut command,
            Some(Instant::now() + Duration::from_millis(15)),
        );
        assert!(outcome.is_err());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let outcome = run_until(
            &mut command,
            Some(Instant::now() - Duration::from_millis(1)),
        );
        assert!(outcome.is_err(), "a step after its deadline does not start");
        Ok(())
    }

    #[test]
    fn a_step_past_its_deadline_ends_with_the_processes_it_started() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pid = directory.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("sleep 60 & echo $! > '{}'; wait", pid.display()));
        let started = Instant::now();
        let error = run_until(&mut command, Some(started + Duration::from_millis(500)))
            .err()
            .map(|error| error.to_string());
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(error.is_some_and(|error| error.contains("deadline passed")));
        // The sleep, a child of the step's shell, went with its group.
        assert!(
            !alive(&fs::read_to_string(&pid)?)?,
            "the step's child outlived the deadline"
        );
        Ok(())
    }

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
    fn daemons_get_no_inherited_descriptors() {
        // An outer shell opens descriptor 7 without close-on-exec, as make
        // leaves its pipe, and runs a program that reports whether it has
        // it: with the wrapper of daemon() it does not.
        let report = r#"if [ -e /dev/fd/7 ]; then echo open; else echo closed; fi"#;
        for shell in ["/bin/sh", "/bin/dash"]
            .into_iter()
            .filter(|shell| Path::new(shell).exists())
        {
            let run = |wrapped: bool| {
                let inner = if wrapped {
                    format!("exec {shell} -c '{CLOSE_AND_EXEC}' {shell} -c '{report}'")
                } else {
                    format!("exec {shell} -c '{report}'")
                };
                let output = Command::new(shell)
                    .arg("-c")
                    .arg(format!("exec 7</dev/null; {inner}"))
                    .output()
                    .unwrap();
                String::from_utf8(output.stdout).unwrap().trim().to_string()
            };
            assert_eq!(run(false), "open", "{shell}");
            assert_eq!(run(true), "closed", "{shell}");
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
    fn clusters_are_named_by_their_spec() {
        let spec = ClusterSpec {
            name: "tool-a".into(),
            database: "a".into(),
            application: "tool".into(),
            conf_name: "tool.conf".into(),
            conf: String::new(),
            port: 5435,
            socket_dir: "/tmp".into(),
            initdb_args: Vec::new(),
            extensions: vec!["tessera".into()],
            stop_hint: "tool stop".into(),
        };
        let cluster = Cluster::new(pg("PostgreSQL 20devel"), Path::new("/runs"), spec);
        assert_eq!(cluster.data, Path::new("/runs/pgdata-tool-a"));
        assert_eq!(cluster.log, Path::new("/runs/tool-a-server.log"));
    }
}
