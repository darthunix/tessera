//! The tool's cluster: tessera-pgtool's cluster with Tessera, the wrapper
//! tessera_parquet and, where the build has it, pg_duckdb, in the data
//! directory `target/bench-runs/pgdata-clickbench`; and in its database
//! the table `hits` twice over the same files: the foreign table
//! `fdw.hits` of `bench/clickbench/create.sql` and pg_duckdb's view
//! `ddb.hits` of `bench/clickbench/view.sql`.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};

pub use tessera_pgtool::Pg;

/// The database the tables are made in.
pub const DATABASE: &str = "clickbench";

/// What the server is started with.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSettings {
    pub port: u16,
    pub socket_dir: String,
    pub shared_buffers: String,
    /// Whether pg_duckdb is preloaded and created.
    pub duckdb: bool,
}

impl ServerSettings {
    /// The configuration file's text, rewritten at every start. The
    /// settings a query runs under are the session's; these are their
    /// defaults, so that psql against the cluster plans like a serial run.
    pub fn conf(&self) -> String {
        let ServerSettings {
            port,
            socket_dir,
            shared_buffers,
            duckdb,
        } = self;
        let preload = if *duckdb {
            "tessera, tessera_nodes, tessera_kernels, pg_duckdb"
        } else {
            "tessera, tessera_nodes, tessera_kernels"
        };
        format!(
            "# Written by tessera-clickbench at every start; changes are lost.
port = {port}
listen_addresses = ''
unix_socket_directories = '{socket_dir}'
shared_preload_libraries = '{preload}'
shared_buffers = {shared_buffers}
# Room for the workers a run asks for, up to 16.
max_worker_processes = 32
max_parallel_workers = 16
# The session sets these per run; the defaults match a serial run.
jit = off
max_parallel_workers_per_gather = 0
work_mem = 64MB
"
        )
    }
}

/// Whether the build has pg_duckdb installed.
pub fn duckdb_installed(pg: &Pg) -> bool {
    pg.sharedir.join("extension/pg_duckdb.control").exists()
        && pg
            .pkglibdir
            .join(format!("pg_duckdb{}", Pg::dlsuffix()))
            .exists()
}

/// The installed files whose hashes say what a run measured: Tessera's,
/// the server's, the wrapper's and pg_duckdb's.
pub fn binaries(pg: &Pg) -> Vec<PathBuf> {
    let mut files = pg.binaries();
    for module in ["tessera_parquet", "pg_duckdb"] {
        files.push(pg.pkglibdir.join(format!("{module}{}", Pg::dlsuffix())));
    }
    files
}

pub struct Cluster {
    cluster: tessera_pgtool::Cluster,
    pub settings: ServerSettings,
}

impl Cluster {
    pub fn new(pg: Pg, runs: &Path, settings: ServerSettings) -> Cluster {
        let mut extensions = vec!["tessera".to_string(), "tessera_parquet".to_string()];
        if settings.duckdb {
            extensions.push("pg_duckdb".into());
        }
        let spec = tessera_pgtool::ClusterSpec {
            name: "clickbench".into(),
            database: DATABASE.into(),
            application: "tessera-clickbench".into(),
            conf_name: "tessera-clickbench.conf".into(),
            conf: settings.conf(),
            port: settings.port,
            socket_dir: settings.socket_dir.clone(),
            // The C locale, as both engines then order strings by their
            // bytes: MIN(URL) and ORDER BY SearchPhrase agree.
            initdb_args: vec!["--no-locale".into()],
            extensions,
            stop_hint: "cargo clickbench stop".into(),
        };
        Cluster {
            cluster: tessera_pgtool::Cluster::new(pg, runs, spec),
            settings,
        }
    }
}

impl Deref for Cluster {
    type Target = tessera_pgtool::Cluster;

    fn deref(&self) -> &tessera_pgtool::Cluster {
        &self.cluster
    }
}

/// The files a table reads: each path a file, or a directory whose
/// `.parquet` files are taken, in the order of the bytes of their names.
pub fn files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        let path = path
            .canonicalize()
            .with_context(|| format!("no file or directory {}", path.display()))?;
        if path.is_dir() {
            let mut inside: Vec<PathBuf> = std::fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<_>>()?;
            inside.retain(|file| file.extension().is_some_and(|ext| ext == "parquet"));
            inside.sort();
            ensure!(
                !inside.is_empty(),
                "{} holds no .parquet file",
                path.display()
            );
            files.extend(inside);
        } else {
            files.push(path);
        }
    }
    for file in &files {
        let text = file.to_str().context("a path that is not UTF-8")?;
        // The wrapper's option files separates its paths by commas,
        // trims spaces and expands the patterns of the shell: a path
        // must name its file alone.
        ensure!(
            !text.contains([',', '*', '?', '[', '\\']) && text.trim() == text,
            "a path with a comma, a character of a pattern or a space at its ends: {text:?}"
        );
    }
    Ok(files)
}

/// A value as an SQL string literal.
pub fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `text` with its one `variable` replaced by `value`, as psql would.
fn substitute(text: &str, variable: &str, value: &str) -> Result<String> {
    ensure!(
        text.matches(variable).count() == 1,
        "{variable} is not in the script once"
    );
    Ok(text.replace(variable, value))
}

/// What the tables were made from, when they were made anew.
pub struct Made {
    pub seconds: f64,
}

/// Makes the foreign table and, with pg_duckdb, its view over `files`,
/// unless the foreign table reads them already, and analyzes the foreign
/// table, as ClickBench's entries load and analyze before timing.
pub fn tables(
    client: &mut postgres::Client,
    root: &Path,
    files: &[PathBuf],
    duckdb: bool,
    deadline: Option<Instant>,
) -> Result<Option<Made>> {
    let list = files
        .iter()
        .map(|file| file.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let read = |name: &str| {
        let path = root.join("bench/clickbench").join(name);
        std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
    };
    let (create, view) = (read("create.sql")?, read("view.sql")?);
    // What the tables were made from: the files, the scripts and whether
    // the view was made, kept as the foreign table's comment.
    // The files by their size and time of change too, so that a file
    // replaced at its path is analyzed again.
    let mut stamp = format!("{create}\n{view}\n{duckdb}\n");
    for file in files {
        let metadata = std::fs::metadata(file)?;
        let changed = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        stamp.push_str(&format!(
            "{} {} {}\n",
            file.display(),
            metadata.len(),
            changed.as_nanos()
        ));
    }
    let made_from = tessera_pgtool::snapshot::digest(stamp.as_bytes());
    let current = client
        .query_one(
            "SELECT obj_description(to_regclass('fdw.hits'), 'pg_class'), \
             (SELECT reltuples FROM pg_class WHERE oid = to_regclass('fdw.hits')), \
             to_regclass('ddb.hits') IS NOT NULL",
            &[],
        )
        .map(|row| {
            (
                row.get::<_, Option<String>>(0),
                row.get::<_, Option<f32>>(1),
                row.get::<_, bool>(2),
            )
        })?;
    // A table whose ANALYZE was stopped has no statistics, and a view
    // dropped since is missing: made anew.
    if current.0.as_deref() == Some(made_from.as_str())
        && current.1.is_some_and(|rows| rows >= 0.0)
        && current.2 == duckdb
    {
        return Ok(None);
    }
    let start = Instant::now();
    let create = substitute(&create, ":'files'", &literal(&list))?;
    let mut statements = vec![
        "DROP SCHEMA IF EXISTS fdw CASCADE".to_string(),
        "DROP SCHEMA IF EXISTS ddb CASCADE".into(),
        "CREATE SCHEMA fdw".into(),
        "CREATE SERVER IF NOT EXISTS clickbench FOREIGN DATA WRAPPER tessera_parquet".into(),
        format!("SET search_path = fdw; {create}; RESET search_path"),
    ];
    if duckdb {
        let array = format!(
            "ARRAY[{}]",
            files
                .iter()
                .map(|file| literal(&file.display().to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let view = substitute(&view, ":files_array", &array)?;
        statements.push("CREATE SCHEMA ddb".into());
        statements.push(format!(
            "SET search_path = ddb, public; {view}; RESET search_path"
        ));
    }
    statements.push("ANALYZE fdw.hits".into());
    statements.push(format!(
        "COMMENT ON FOREIGN TABLE fdw.hits IS {}",
        literal(&made_from)
    ));
    for statement in &statements {
        // ANALYZE of a large file takes long: the run's deadline stops it,
        // and every statement before it.
        if let Some(deadline) = deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !left.is_zero(),
                "the deadline passed while the tables were made"
            );
            client.batch_execute(&format!(
                "SET statement_timeout = {}",
                left.as_millis().max(1)
            ))?;
        }
        client.batch_execute(statement)?;
    }
    client.batch_execute("RESET statement_timeout")?;
    Ok(Some(Made {
        seconds: start.elapsed().as_secs_f64(),
    }))
}

/// The rows of the foreign table and the version of pg_duckdb, for the
/// run's record.
pub fn describe(client: &mut postgres::Client) -> Result<(i64, Option<String>)> {
    let rows: f32 = client
        .query_one(
            "SELECT reltuples FROM pg_class WHERE oid = 'fdw.hits'::regclass",
            &[],
        )?
        .get(0);
    let duckdb: Option<String> = client
        .query_opt(
            "SELECT extversion FROM pg_extension WHERE extname = 'pg_duckdb'",
            &[],
        )?
        .map(|row| row.get(0));
    if rows < 0.0 {
        bail!("fdw.hits was never analyzed");
    }
    Ok((rows as i64, duckdb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preload_follows_pg_duckdb() {
        let mut settings = ServerSettings {
            port: 5439,
            socket_dir: "/tmp".into(),
            shared_buffers: "2GB".into(),
            duckdb: true,
        };
        assert!(settings.conf().contains(
            "shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels, pg_duckdb'"
        ));
        settings.duckdb = false;
        assert!(
            settings
                .conf()
                .contains("shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'\n")
        );
        assert!(settings.conf().contains("port = 5439\n"));
    }

    #[test]
    fn directories_give_their_parquet_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "hits_10.parquet",
            "hits_0.parquet",
            "notes.txt",
            "hits_9.parquet",
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let found = files(&[dir.path().to_path_buf()]).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|file| file.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["hits_0.parquet", "hits_10.parquet", "hits_9.parquet"]
        );
        let one = files(&[dir.path().join("hits_9.parquet")]).unwrap();
        assert_eq!(one.len(), 1);
        assert!(one[0].is_absolute());
        assert!(files(&[dir.path().join("none.parquet")]).is_err());
        for name in [
            "a,b.parquet",
            "a*.parquet",
            "a?.parquet",
            "[a].parquet",
            "a\\b.parquet",
            "a.parquet ",
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"").unwrap();
            assert!(files(&[path]).is_err(), "{name:?}");
        }
        let empty = tempfile::tempdir().unwrap();
        assert!(files(&[empty.path().to_path_buf()]).is_err());
    }

    #[test]
    fn scripts_take_their_variables_once() {
        assert_eq!(
            substitute("OPTIONS (files :'files')", ":'files'", &literal("/a/it's")).unwrap(),
            "OPTIONS (files '/a/it''s')"
        );
        assert!(substitute("no variable", ":'files'", "x").is_err());
        assert!(substitute(":'files' :'files'", ":'files'", "x").is_err());
    }

    #[test]
    fn the_scripts_have_their_variables() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/clickbench");
        let create = std::fs::read_to_string(root.join("create.sql")).unwrap();
        let view = std::fs::read_to_string(root.join("view.sql")).unwrap();
        assert!(substitute(&create, ":'files'", "'x'").is_ok());
        assert!(substitute(&view, ":files_array", "ARRAY['x']").is_ok());
        // The same 105 columns, in the same order.
        let names = |text: &str, pattern: fn(&str) -> Option<String>| -> Vec<String> {
            text.lines().filter_map(pattern).collect()
        };
        let table = names(&create, |line| {
            line.contains("OPTIONS (column")
                .then(|| line.split_whitespace().next().unwrap().to_string())
        });
        let columns = names(&view, |line| {
            line.trim_end_matches(',')
                .rsplit_once(" AS ")
                .filter(|_| line.starts_with("    "))
                .map(|(_, name)| name.to_string())
        });
        assert_eq!(table.len(), 105);
        assert_eq!(table, columns);
    }
}
