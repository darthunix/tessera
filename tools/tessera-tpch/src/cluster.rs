//! The cluster of one scale factor: tessera-pgtool's cluster with the
//! settings, the database and the contrib modules of TPC-H, a data
//! directory per scale factor under `target/bench-runs/`.

use std::ops::Deref;
use std::path::Path;

pub use tessera_pgtool::{Pg, install_tessera};

use crate::config::{Scale, ServerSettings};

/// The contrib modules the tool needs: `pg_prewarm` reads the tables into
/// shared buffers before a timed run, `pg_buffercache` checks that they
/// stayed there.
pub const CONTRIB: [&str; 2] = ["pg_prewarm", "pg_buffercache"];

/// The database the data is loaded into.
pub const DATABASE: &str = "tpch";

/// The cluster of one scale factor.
pub struct Cluster {
    cluster: tessera_pgtool::Cluster,
    pub settings: ServerSettings,
}

impl Cluster {
    pub fn new(pg: Pg, runs: &Path, scale: &Scale, settings: ServerSettings) -> Cluster {
        let spec = tessera_pgtool::ClusterSpec {
            name: format!("tpch-sf{scale}"),
            database: DATABASE.into(),
            application: "tessera-tpch".into(),
            conf_name: "tessera-tpch.conf".into(),
            conf: settings.conf(),
            port: settings.port,
            socket_dir: settings.socket_dir.clone(),
            // The C locale: the answers of TPC-H sort strings by their
            // bytes, and both modes compare them under one collation.
            initdb_args: vec!["--no-locale".into()],
            extensions: std::iter::once("tessera")
                .chain(CONTRIB)
                .map(String::from)
                .collect(),
            stop_hint: "cargo tpch stop --sf <its scale>".into(),
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
