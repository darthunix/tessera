//! `cargo tpch`: the 22 queries derived from TPC-H on a PostgreSQL cluster
//! of the tool's own, with Tessera on and off. See `bench/tpch/README.md`.
#![forbid(unsafe_code)]

mod cluster;
mod config;
mod load;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use cluster::{Cluster, Pg};
use config::{Scale, ServerSettings};
use load::Schema;

#[derive(Debug, Parser)]
#[command(
    name = "tessera-tpch",
    about = "Queries derived from TPC-H on a PostgreSQL cluster of its own, with Tessera on and off",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install the release build of Tessera, start the cluster of the
    /// scale factor, load the data unless it holds them, and leave it
    /// running.
    Setup(ClusterArgs),
    /// Stop the cluster of the scale factor; its data stays.
    Stop(ClusterArgs),
}

/// The cluster: one per scale factor, under target/bench-runs/.
#[derive(Debug, Clone, Args)]
struct ClusterArgs {
    /// Scale factor: 1 is 6 M rows of lineitem, about 1 GB of raw data.
    #[arg(long, default_value = "1", value_parser = Scale::parse)]
    sf: Scale,
    /// Port of the cluster's server.
    #[arg(long, default_value_t = 5434)]
    port: u16,
    /// shared_buffers of the server [default: 1.6 GB per scale unit, at
    /// least 2 GB].
    #[arg(long, value_name = "SIZE")]
    shared_buffers: Option<String>,
    /// Keep the Tessera build installed in PG_CONFIG's tree instead of
    /// building and installing the release one.
    #[arg(long)]
    no_install: bool,
    /// Variant of the schema: primary keys only, or with indexes on
    /// foreign keys and dates (bench/tpch/indexes.sql).
    #[arg(long, value_enum, default_value_t = Schema::Pk)]
    schema: Schema,
    /// Load the data anew even if the cluster holds them.
    #[arg(long)]
    reload: bool,
}

impl ClusterArgs {
    fn settings(&self) -> ServerSettings {
        ServerSettings {
            port: self.port,
            socket_dir: "/tmp".into(),
            shared_buffers: self
                .shared_buffers
                .clone()
                .unwrap_or_else(|| self.sf.default_shared_buffers()),
        }
    }

    fn cluster(&self, pg: Pg) -> Cluster {
        Cluster::new(pg, &runs_dir(), &self.sf, self.settings())
    }
}

/// The repository's root, which holds the queries and the run directories.
fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.canonicalize().unwrap_or(root)
}

/// Where clusters and runs live, as with every measurement of the
/// repository: git ignores it.
fn runs_dir() -> PathBuf {
    root().join("target/bench-runs")
}

/// Installs Tessera unless asked not to, starts the cluster and loads
/// the data it lacks.
fn bring_up(args: &ClusterArgs) -> Result<Cluster> {
    let pg = Pg::discover()?;
    pg.require_contrib()?;
    std::fs::create_dir_all(runs_dir())?;
    if args.no_install {
        println!("Tessera: the build installed in {}", pg.pkglibdir.display());
    } else {
        let log = runs_dir().join("tpch-install.log");
        println!(
            "Tessera: building the release libraries and installing them (log {})",
            log.display()
        );
        cluster::install_tessera(&root(), &pg, &log)?;
    }
    let cluster = args.cluster(pg);
    cluster.start()?;
    println!(
        "cluster: {} on port {}, shared_buffers {}",
        cluster.data.display(),
        cluster.settings.port,
        cluster.settings.shared_buffers
    );
    match load::ensure(&cluster, &root(), &args.sf, args.schema, args.reload)? {
        Some(loaded) => {
            println!(
                "loaded in {:.0} s; rows as clause 4.2.5 gives them:",
                loaded.seconds
            );
            for table in &loaded.tables {
                println!(
                    "  {:>9} {:>11} rows {:>9} pages, {} all-visible",
                    table.table.name(),
                    table.rows,
                    table.pages,
                    table.all_visible
                );
            }
        }
        None => println!("data: SF {} of {} is loaded", args.sf, load::GENERATOR),
    }
    let mut client = cluster.connect()?;
    let sizes = load::sizes(&mut client)?;
    let total: i64 = sizes.iter().map(|(_, size)| size).sum();
    println!(
        "schema {}, {} with indexes: {}",
        args.schema.name(),
        load::human_size(total),
        sizes
            .iter()
            .map(|(table, size)| format!("{table} {}", load::human_size(*size)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(cluster)
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Setup(args) => {
            bring_up(&args)?;
            println!(
                "the server runs; psql -h /tmp -p {} {}; stop it with cargo tpch stop --sf {}",
                args.port,
                cluster::DATABASE,
                args.sf
            );
        }
        Command::Stop(args) => {
            let cluster = args.cluster(Pg::discover()?);
            if cluster.stop()? {
                println!("stopped {}", cluster.data.display());
            } else {
                println!("{} does not run", cluster.data.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_interface_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn setup_takes_the_cluster_arguments() {
        let cli = Cli::try_parse_from([
            "tessera-tpch",
            "setup",
            "--sf",
            "10",
            "--port",
            "5500",
            "--no-install",
            "--schema",
            "indexed",
        ])
        .unwrap();
        let Command::Setup(args) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.sf.to_string(), "10");
        assert!(args.no_install);
        assert_eq!(args.schema, Schema::Indexed);
        let settings = args.settings();
        assert_eq!(settings.port, 5500);
        assert_eq!(settings.shared_buffers, "16GB");
    }

    #[test]
    fn shared_buffers_can_be_set() {
        let cli = Cli::try_parse_from(["tessera-tpch", "stop", "--shared-buffers", "4GB"]).unwrap();
        let Command::Stop(args) = cli.command else {
            panic!("{cli:?}");
        };
        assert_eq!(args.sf.to_string(), "1");
        assert_eq!(args.settings().shared_buffers, "4GB");
    }

    #[test]
    fn a_bad_scale_is_refused() {
        assert!(Cli::try_parse_from(["tessera-tpch", "setup", "--sf", "-1"]).is_err());
    }
}
