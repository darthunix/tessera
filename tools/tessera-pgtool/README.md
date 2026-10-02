# tessera-pgtool

A small library, not a program, that the tools which run SQL against
Tessera share: [`cargo tpch`](../../bench/tpch/README.md) and
[`tessera-crosscheck`](../tessera-crosscheck/README.md). It does three
things for a tool:

- finds a PostgreSQL build by its `pg_config`;
- builds the release libraries of Tessera and installs them into that build;
- creates, starts, stops and connects to a cluster of the tool's own, with
  the Tessera modules preloaded.

## Why it exists

A tool that measures or checks Tessera needs a server that runs exactly the
build it just installed, and nothing else on the machine. `tessera-tpch`
grew that code first. When `tessera-crosscheck` needed the same, the code
moved here instead of being copied. Each rule below was a bug once, so it
stays in one place:

- **The server restarts at every start.** The postmaster loads the modules
  once and forks every backend from itself, so a server left running keeps
  the previous build. `Cluster::start` restarts a running server.
- **The release build is installed.** A test run leaves a debug build of
  the Rust kernels installed, and a timing would measure it.
  `install_tessera` builds and installs `RUST_PROFILE=release`.
- **The server inherits no descriptors.** The postmaster outlives the tool.
  Started from `make tpch`, it kept make's pipe open and make waited
  forever. The server is started through `sh`, which closes descriptors 3
  to 9 first.
- **A taken port names its owner.** When another server holds the port, the
  error gives that server's data directory, read from the socket's lock
  file, and how to stop it.
- **A data directory matches the build.** A cluster made by another major
  version of PostgreSQL is refused, not started.
- **A nested make keeps its own flags.** `install_tessera` removes
  `MAKEFLAGS` and the jobserver of an outer make, which would clash with
  its own `-j`.

## What is on disk

A tool passes its run directory, usually `target/bench-runs`, and a cluster
name. For the name `crosscheck` the files are:

| Path | What |
|---|---|
| `pgdata-crosscheck/` | the data directory |
| `pgdata-crosscheck/<conf_name>` | the tool's settings, rewritten at every start; `postgresql.conf` includes it |
| `pgdata-crosscheck.initdb.log` | initdb's output |
| `crosscheck-server.log` | the server's log |

initdb makes the cluster with trust authentication and UTF8, and the
operating system's user is its superuser. The library connects over the
Unix socket of the spec's directory; both tools also turn TCP off with
`listen_addresses = ''`. The tool's database is created at the first start.
Its extensions are created at every start, with `IF NOT EXISTS`.

## Using it from a tool

Add the crate to the tool's `Cargo.toml`:

```toml
[dependencies]
tessera-pgtool.workspace = true
```

Then describe the cluster and start it. This is the shape of
`tessera-crosscheck`'s code:

```rust
use tessera_pgtool::{Cluster, ClusterSpec, Pg, install_tessera};

let pg = Pg::discover()?;                       // PG_CONFIG, else pg_config in PATH
install_tessera(&root, &pg, &runs.join("install.log"))?;

let spec = ClusterSpec {
    name: "mytool".into(),
    database: "mytool".into(),
    application: "tessera-mytool".into(),
    conf_name: "tessera-mytool.conf".into(),
    conf: "port = 5436\nlisten_addresses = ''\nunix_socket_directories = '/tmp'\n\
           shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'\n"
        .into(),
    port: 5436,
    socket_dir: "/tmp".into(),
    initdb_args: vec!["--no-locale".into()],
    extensions: vec!["tessera".into()],
    stop_hint: "cargo run -p tessera-mytool -- stop".into(),
};
let cluster = Cluster::new(pg, &runs, spec);
let was_running = cluster.start()?;              // initdb if needed, then start or restart
let mut client = cluster.connect()?;             // a postgres::Client on the tool's database
```

The settings in `conf` are the tool's to choose. They must name the port,
the socket directory and the preloaded modules, because those need a
restart. `port` and `socket_dir` in the spec must agree with them.

A tool should also have a `stop` command that calls `Cluster::stop`.
Stopping keeps the data, and the next start reuses it. To start over,
stop the server and delete the data directory.

## The interface

| Item | What it does |
|---|---|
| `Pg::discover()` | Reads `PG_CONFIG`, else runs `pg_config` from `PATH`, and returns the build's directories and version. |
| `Pg::major()` | The major version, such as `20`. |
| `Pg::bin(name)` | The path of a program in the build's `bin`. |
| `Pg::binaries()` | The installed Tessera modules and `postgres`, whose hashes record which build a run used. |
| `Pg::require_contrib(names)` | Fails with the `make … install` commands for the contrib modules the build lacks. |
| `install_tessera(root, pg, log)` | Runs `make` and `make install` with `RUST_PROFILE=release` into `pg`; the output goes to `log`. |
| `ClusterSpec` | The cluster's name, database, application name, settings file, port, socket directory, initdb arguments, extensions and stop hint. |
| `Cluster::start()` | Creates the cluster if needed, writes the settings, starts or restarts the server, creates the database and extensions. Returns whether the server ran before. |
| `Cluster::stop()` | Stops the server if it runs. Returns whether it did. |
| `Cluster::running()` | Whether the server of the data directory runs. |
| `Cluster::connect()`, `connect_to(db)` | A connection over the socket as the operating system's user. |
| `message(error)` | A client error as `SQLSTATE: message`. |
| `tail(path, lines)` | The last lines of a log, for an error message. |

## Who uses it

| Tool | Cluster | Port | Database | Extensions |
|---|---|---|---|---|
| `cargo tpch` | `tpch-sf<scale>`, one per scale factor | 5434 | `tpch` | tessera, pg_prewarm, pg_buffercache |
| `tessera-crosscheck` | `crosscheck` | 5435 | `crosscheck` | tessera |

Both create their clusters with `--no-locale`, so that text compares by its
bytes in both modes, and both can skip the install with `--no-install` to
use the build already installed.

## Requirements and limits

- A PostgreSQL build that Tessera supports, PostgreSQL master, named by
  `PG_CONFIG`.
- macOS or Linux. Connections go over a Unix socket, with no TCP and no
  TLS.
- One machine: the tool, the build and the cluster are local.

## Tests

```sh
cargo test -p tessera-pgtool
```

The unit tests cover the descriptors a started server inherits, version
parsing, the installed file list and the cluster's paths. The tools that
use the crate exercise the rest against a real server.
