//! What the cluster is started with: the scale factor and the server's
//! settings. The settings a query runs under (Tessera on or off, workers,
//! `work_mem`) are set per connection instead, so that changing them needs
//! no restart.

use std::fmt;

use anyhow::{Result, bail};

/// A TPC-H scale factor: SF 1 is about 1 GB of raw data and 6 M rows of
/// `lineitem`. The label is its canonical decimal form, which names the
/// cluster's directory, so `1`, `1.0` and `01` are one cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct Scale {
    factor: f64,
    label: String,
}

impl Scale {
    /// Parses a positive decimal number without an exponent.
    pub fn parse(text: &str) -> Result<Scale> {
        let text = text.trim();
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty() && fraction.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            bail!("a scale factor is a decimal number such as 1, 10 or 0.01, not {text:?}");
        }
        let whole = whole.trim_start_matches('0');
        let fraction = fraction.trim_end_matches('0');
        let whole = if whole.is_empty() { "0" } else { whole };
        let label = if fraction.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{fraction}")
        };
        let factor: f64 = label.parse()?;
        if factor <= 0.0 {
            bail!("the scale factor must be above zero");
        }
        Ok(Scale { factor, label })
    }

    pub fn factor(&self) -> f64 {
        self.factor
    }

    /// `shared_buffers` that holds every table and primary key with room
    /// to spare: 1.6 GB per scale unit, at least 2 GB (2 GB at SF 1, 16 GB
    /// at SF 10), so that a timed query reads no page from the operating
    /// system's cache.
    pub fn default_shared_buffers(&self) -> String {
        let gigabytes = (self.factor * 1.6).ceil().max(2.0);
        format!("{gigabytes}GB")
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// The settings that need the server's restart, written to a file of their
/// own that `postgresql.conf` includes, and rewritten at every start.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSettings {
    pub port: u16,
    /// The directory of the server's socket.
    pub socket_dir: String,
    pub shared_buffers: String,
}

impl ServerSettings {
    /// The configuration file's text. The values a query runs under are
    /// the session's (see the check and the timing); these are their
    /// defaults, so that `psql` against the cluster plans like them.
    pub fn conf(&self) -> String {
        let ServerSettings {
            port,
            socket_dir,
            shared_buffers,
        } = self;
        format!(
            "# Written by tessera-tpch at every start; changes are lost.
port = {port}
listen_addresses = ''
unix_socket_directories = '{socket_dir}'
shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'
shared_buffers = {shared_buffers}
# The session sets these per run; the defaults match a serial run.
jit = off
max_parallel_workers_per_gather = 0
work_mem = 256MB
# The data is loaded once and frozen; nothing may vacuum during a run.
autovacuum = off
# The bulk load writes gigabytes of WAL: no checkpoint in the middle of it.
max_wal_size = 32GB
checkpoint_timeout = 1h
synchronous_commit = off
"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_labels_are_canonical() {
        for (text, label, factor) in [
            ("1", "1", 1.0),
            ("1.0", "1", 1.0),
            ("01", "1", 1.0),
            ("10", "10", 10.0),
            ("0.01", "0.01", 0.01),
            (".1", "0.1", 0.1),
            ("0.010", "0.01", 0.01),
            (" 3 ", "3", 3.0),
        ] {
            let scale = Scale::parse(text).unwrap();
            assert_eq!(scale.to_string(), label, "{text}");
            assert_eq!(scale.factor, factor, "{text}");
        }
    }

    #[test]
    fn scale_rejects_what_is_not_a_positive_decimal() {
        for text in ["", ".", "0", "0.0", "-1", "1e3", "1.2.3", "ten", "inf"] {
            assert!(Scale::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn shared_buffers_hold_the_data() {
        let buffers = |text| Scale::parse(text).unwrap().default_shared_buffers();
        assert_eq!(buffers("0.01"), "2GB");
        assert_eq!(buffers("1"), "2GB");
        assert_eq!(buffers("3"), "5GB");
        assert_eq!(buffers("10"), "16GB");
        assert_eq!(buffers("100"), "160GB");
    }

    #[test]
    fn conf_preloads_tessera_and_sets_the_server() {
        let conf = ServerSettings {
            port: 5434,
            socket_dir: "/tmp".into(),
            shared_buffers: "2GB".into(),
        }
        .conf();
        for line in [
            "port = 5434",
            "unix_socket_directories = '/tmp'",
            "shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'",
            "shared_buffers = 2GB",
            "jit = off",
            "autovacuum = off",
        ] {
            assert!(conf.lines().any(|l| l == line), "{line}\n{conf}");
        }
    }
}
