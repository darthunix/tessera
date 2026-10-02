//! The data: generated in the process by the `tpchgen` crate, which
//! produces the rows of dbgen byte for byte (its CI compares them at SF 1,
//! 10 and 100), and copied into the tables, each through one connection
//! of its own and the tables at once.
//!
//! One connection per table keeps its rows in the order of their keys, as
//! a load of dbgen's files has them, and the same at every load: several
//! COPY into one table interleave their rows over its pages, differently
//! each time, which changes the correlation of its indexes and so the
//! plans, and leave pages that bulk extension added and nobody filled,
//! which VACUUM never marks all-visible.

use std::collections::BTreeMap;
use std::fmt::{Display, Write as _};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tpchgen::generators::{
    CustomerGenerator, LineItemGenerator, NationGenerator, OrderGenerator, PartGenerator,
    PartSuppGenerator, RegionGenerator, SupplierGenerator,
};

use crate::cluster::Cluster;
use crate::config::Scale;

/// The generator, as the load's label records it. Data loaded by another
/// generator is not the same data, so a change here reloads it.
pub const GENERATOR: &str = "tpchgen 3.0";

/// The variant of the schema: the primary keys only, or with indexes on
/// foreign keys and dates as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Schema {
    /// Primary keys only: every table is read by a sequential scan.
    Pk,
    /// Primary keys and the indexes of bench/tpch/indexes.sql.
    Indexed,
}

impl Schema {
    pub fn name(self) -> &'static str {
        match self {
            Schema::Pk => "pk",
            Schema::Indexed => "indexed",
        }
    }
}

/// A table of TPC-H.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Table {
    Region,
    Nation,
    Part,
    Supplier,
    PartSupp,
    Customer,
    Orders,
    LineItem,
}

impl Table {
    pub const ALL: [Table; 8] = [
        Table::Region,
        Table::Nation,
        Table::Part,
        Table::Supplier,
        Table::PartSupp,
        Table::Customer,
        Table::Orders,
        Table::LineItem,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Table::Region => "region",
            Table::Nation => "nation",
            Table::Part => "part",
            Table::Supplier => "supplier",
            Table::PartSupp => "partsupp",
            Table::Customer => "customer",
            Table::Orders => "orders",
            Table::LineItem => "lineitem",
        }
    }

    /// The rows of clause 4.2.5 at a scale factor; lineitem's count is
    /// only approximately four per order, so it is known at the scales
    /// whose count is published.
    pub fn expected_rows(self, sf: f64) -> Option<i64> {
        Some(match self {
            Table::Region => 5,
            Table::Nation => 25,
            Table::Part => PartGenerator::calculate_row_count(sf, 1, 1),
            Table::Supplier => SupplierGenerator::calculate_row_count(sf, 1, 1),
            // The generator counts parts here; each has four suppliers.
            Table::PartSupp => 4 * PartSuppGenerator::calculate_row_count(sf, 1, 1),
            Table::Customer => CustomerGenerator::calculate_row_count(sf, 1, 1),
            Table::Orders => OrderGenerator::calculate_row_count(sf, 1, 1),
            Table::LineItem => {
                return [
                    (0.01, 60_175),
                    (1.0, 6_001_215),
                    (10.0, 59_986_052),
                    (100.0, 600_037_902),
                ]
                .into_iter()
                .find(|&(scale, _)| scale == sf)
                .map(|(_, rows)| rows);
            }
        })
    }

    /// The rows the progress bar expects: about four lines per order.
    fn estimated_rows(self, sf: f64) -> u64 {
        let rows = self
            .expected_rows(sf)
            .unwrap_or_else(|| 4 * OrderGenerator::calculate_row_count(sf, 1, 1));
        rows.max(1) as u64
    }

    /// Generates the table into `sink`, in COPY's text format.
    fn generate(self, sf: f64, sink: &mut CopySink<'_>) -> Result<()> {
        match self {
            Table::Region => sink.rows(RegionGenerator::new(sf, 1, 1).iter()),
            Table::Nation => sink.rows(NationGenerator::new(sf, 1, 1).iter()),
            Table::Part => sink.rows(PartGenerator::new(sf, 1, 1).iter()),
            Table::Supplier => sink.rows(SupplierGenerator::new(sf, 1, 1).iter()),
            Table::PartSupp => sink.rows(PartSuppGenerator::new(sf, 1, 1).iter()),
            Table::Customer => sink.rows(CustomerGenerator::new(sf, 1, 1).iter()),
            Table::Orders => sink.rows(OrderGenerator::new(sf, 1, 1).iter()),
            Table::LineItem => sink.rows(LineItemGenerator::new(sf, 1, 1).iter()),
        }
    }
}

/// Appends a row of dbgen's format, fields ended by `|`, as a line of
/// COPY's text format with `|` between the fields: without the last `|`,
/// and with the characters text format reserves escaped (dbgen produces
/// none of them, so this only keeps a change of generator from loading
/// wrong rows silently).
pub fn push_copy_line(buffer: &mut Vec<u8>, row: &str) {
    let row = row.strip_suffix('|').unwrap_or(row);
    for byte in row.bytes() {
        match byte {
            b'\\' => buffer.extend_from_slice(b"\\\\"),
            b'\n' => buffer.extend_from_slice(b"\\n"),
            b'\r' => buffer.extend_from_slice(b"\\r"),
            _ => buffer.push(byte),
        }
    }
    buffer.push(b'\n');
}

/// Rows formatted into a buffer that is written to a COPY in pieces of
/// about a megabyte.
struct CopySink<'a> {
    writer: &'a mut dyn Write,
    progress: &'a ProgressBar,
    buffer: Vec<u8>,
    line: String,
    rows: u64,
}

impl CopySink<'_> {
    const FLUSH: usize = 1 << 20;

    fn rows<R: Display>(&mut self, rows: impl Iterator<Item = R>) -> Result<()> {
        let mut pending = 0;
        for row in rows {
            self.line.clear();
            write!(self.line, "{row}")?;
            push_copy_line(&mut self.buffer, &self.line);
            self.rows += 1;
            pending += 1;
            if self.buffer.len() >= Self::FLUSH {
                self.writer.write_all(&self.buffer)?;
                self.buffer.clear();
                self.progress.inc(pending);
                pending = 0;
            }
        }
        self.writer.write_all(&self.buffer)?;
        self.buffer.clear();
        self.progress.inc(pending);
        Ok(())
    }
}

/// What a load found: the rows of every table as the generator wrote them
/// and as the table counts them.
#[derive(Debug)]
pub struct Loaded {
    pub seconds: f64,
    pub tables: Vec<TableLoad>,
}

/// A table after the load: its rows, and its pages with how many of them
/// the visibility map marks all-visible, which spares a scan the
/// visibility check of every row.
#[derive(Debug)]
pub struct TableLoad {
    pub table: Table,
    pub rows: i64,
    pub pages: i64,
    pub all_visible: i64,
}

/// Loads the data unless the database holds this scale's already (or
/// `reload` asks for a new load), then gives the schema its variant.
/// Returns the load when there was one.
pub fn ensure(
    cluster: &Cluster,
    root: &Path,
    scale: &Scale,
    schema: Schema,
    reload: bool,
) -> Result<Option<Loaded>> {
    let mut client = cluster.connect()?;
    let labelled: bool = client
        .query_one("SELECT to_regclass('tpch_meta') IS NOT NULL", &[])?
        .get(0);
    let label: Option<String> = if labelled {
        client
            .query_opt("SELECT scale || ' ' || generator FROM tpch_meta", &[])?
            .map(|row| row.get(0))
    } else {
        None
    };
    let wanted = format!("{scale} {GENERATOR}");
    let loaded = if !reload && label.as_deref() == Some(wanted.as_str()) {
        None
    } else {
        if let Some(label) = label.filter(|label| *label != wanted) {
            println!("the database holds {label}, not {wanted}: loading anew");
        }
        Some(load(cluster, &mut client, root, scale)?)
    };
    apply_schema(&mut client, root, schema)?;
    Ok(loaded)
}

fn load(
    cluster: &Cluster,
    client: &mut postgres::Client,
    root: &Path,
    scale: &Scale,
) -> Result<Loaded> {
    let start = Instant::now();
    let sql = |name: &str| {
        let path = root.join("bench/tpch").join(name);
        std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
    };
    let mut drop = String::from("DROP TABLE IF EXISTS tpch_meta");
    for table in Table::ALL {
        write!(drop, ", {}", table.name())?;
    }
    client.batch_execute(&drop)?;
    client.batch_execute(&sql("schema.sql")?)?;

    let sf = scale.factor();
    let generated = copy_all(cluster, sf)?;

    let keys = Instant::now();
    println!("building the primary keys");
    client.batch_execute("SET maintenance_work_mem = '1GB'")?;
    client.batch_execute(&sql("keys.sql")?)?;
    println!("  {:.1} s", keys.elapsed().as_secs_f64());
    // Frozen and analysed: the first reads of a table would write its
    // hint bits otherwise, and autovacuum is off. A second VACUUM, because
    // the first after a COPY freezes the rows but marks no page
    // all-visible on PostgreSQL master (seen on 311df1dc039), and the
    // second does.
    let vacuum = Instant::now();
    println!("VACUUM (FREEZE, ANALYZE), then VACUUM");
    for table in Table::ALL {
        client.batch_execute(&format!("VACUUM (FREEZE, ANALYZE) {}", table.name()))?;
        client.batch_execute(&format!("VACUUM {}", table.name()))?;
    }
    client.batch_execute("CHECKPOINT")?;
    println!("  {:.1} s", vacuum.elapsed().as_secs_f64());

    let mut tables = Vec::new();
    for table in Table::ALL {
        let counted: i64 = client
            .query_one(&format!("SELECT count(*) FROM {}", table.name()), &[])?
            .get(0);
        let written = generated[&table];
        if counted != written {
            bail!(
                "{} counts {counted} rows, the generator wrote {written}",
                table.name()
            );
        }
        if let Some(expected) = table.expected_rows(sf)
            && counted != expected
        {
            bail!(
                "{} has {counted} rows, clause 4.2.5 gives {expected} at SF {scale}",
                table.name()
            );
        }
        let pages = client.query_one(
            "SELECT relpages::int8, relallvisible::int8 FROM pg_class WHERE oid = $1::text::regclass",
            &[&table.name()],
        )?;
        tables.push(TableLoad {
            table,
            rows: counted,
            pages: pages.get(0),
            all_visible: pages.get(1),
        });
    }
    let seconds = start.elapsed().as_secs_f64();
    client.batch_execute(&format!(
        "CREATE TABLE tpch_meta (scale text NOT NULL, generator text NOT NULL,
                                 loaded timestamptz NOT NULL, seconds numeric NOT NULL);
         INSERT INTO tpch_meta VALUES ('{scale}', '{GENERATOR}', now(), {seconds:.1});"
    ))?;
    Ok(Loaded { seconds, tables })
}

/// Copies every table through a connection of its own, all at once, and
/// returns the rows each received.
fn copy_all(cluster: &Cluster, sf: f64) -> Result<BTreeMap<Table, i64>> {
    let bars = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "{prefix:>9} [{bar:30}] {human_pos:>11}/{human_len:<11} {per_sec:>14}",
    )?
    .progress_chars("=> ");
    std::thread::scope(|scope| -> Result<BTreeMap<Table, i64>> {
        let copies: Vec<_> = Table::ALL
            .into_iter()
            .map(|table| {
                let bar = bars.add(ProgressBar::new(table.estimated_rows(sf)));
                bar.set_style(style.clone());
                bar.set_prefix(table.name());
                let copy = scope.spawn(move || -> Result<i64> {
                    let mut client = cluster.connect()?;
                    let sql = format!(
                        "COPY {} FROM STDIN (FORMAT text, DELIMITER '|')",
                        table.name()
                    );
                    let mut writer = client.copy_in(&sql)?;
                    let mut sink = CopySink {
                        writer: &mut writer,
                        progress: &bar,
                        buffer: Vec::with_capacity(CopySink::FLUSH + 4096),
                        line: String::new(),
                        rows: 0,
                    };
                    table.generate(sf, &mut sink)?;
                    let generated = sink.rows;
                    let copied = writer.finish()?;
                    if copied != generated {
                        bail!("{}: copied {copied} rows of {generated}", table.name());
                    }
                    bar.set_length(copied);
                    bar.finish();
                    Ok(copied as i64)
                });
                (table, copy)
            })
            .collect();
        copies
            .into_iter()
            .map(|(table, copy)| {
                let copied = copy
                    .join()
                    .map_err(|_| anyhow::anyhow!("the thread loading {table:?} panicked"))??;
                Ok((table, copied))
            })
            .collect()
    })
}

/// Creates the indexes of the variant, or drops every index that is not a
/// primary key; either is a no-op when the database already has it.
fn apply_schema(client: &mut postgres::Client, root: &Path, schema: Schema) -> Result<()> {
    match schema {
        Schema::Indexed => {
            let path = root.join("bench/tpch/indexes.sql");
            let sql = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            client.batch_execute("SET maintenance_work_mem = '1GB'")?;
            client.batch_execute(&sql)?;
        }
        Schema::Pk => {
            let extra: Vec<String> = client
                .query(
                    "SELECT i.indexrelid::regclass::text
                       FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid
                      WHERE NOT i.indisprimary AND c.relnamespace = 'public'::regnamespace",
                    &[],
                )?
                .into_iter()
                .map(|row| row.get(0))
                .collect();
            for index in extra {
                client.batch_execute(&format!("DROP INDEX {index}"))?;
            }
        }
    }
    Ok(())
}

/// The size of every table with its indexes, in bytes.
pub fn sizes(client: &mut postgres::Client) -> Result<Vec<(String, i64)>> {
    let mut sizes = Vec::new();
    for table in Table::ALL {
        let size: i64 = client
            .query_one(
                "SELECT pg_total_relation_size($1::text::regclass)",
                &[&table.name()],
            )?
            .get(0);
        sizes.push((table.name().to_string(), size));
    }
    Ok(sizes)
}

/// A size as pg_size_pretty prints it: the next unit once the number
/// reaches 10240, rounded half up.
pub fn human_size(bytes: i64) -> String {
    let mut value = bytes;
    for unit in ["bytes", "kB", "MB", "GB"] {
        if value.abs() < 10 * 1024 {
            return format!("{value} {unit}");
        }
        value = (value + 512) / 1024;
    }
    format!("{value} TB")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first row of every table at SF 0.01 as dbgen writes it, from
    /// the data the tpchgen crate is tested against.
    const FIRST_ROWS: [(Table, &str); 8] = [
        (
            Table::Region,
            "0|AFRICA|lar deposits. blithely final packages cajole. regular waters are final requests. regular accounts are according to |",
        ),
        (
            Table::Nation,
            "0|ALGERIA|0| haggle. carefully final deposits detect slyly agai|",
        ),
        (
            Table::Part,
            "1|goldenrod lavender spring chocolate lace|Manufacturer#1|Brand#13|PROMO BURNISHED COPPER|7|JUMBO PKG|901.00|ly. slyly ironi|",
        ),
        (
            Table::Supplier,
            "1|Supplier#000000001| N kD4on9OM Ipw3,gf0JBoQDd7tgrzrddZ|17|27-918-335-1736|5755.94|each slyly above the careful|",
        ),
        (
            Table::PartSupp,
            "1|2|3325|771.64|, even theodolites. regular, final theodolites eat after the carefully pending foxes. furiously regular deposits sleep slyly. carefully bold realms above the ironic dependencies haggle careful|",
        ),
        (
            Table::Customer,
            "1|Customer#000000001|IVhzIApeRb ot,c,E|15|25-989-741-2988|711.56|BUILDING|to the even, regular platelets. regular, ironic epitaphs nag e|",
        ),
        (
            Table::Orders,
            "1|370|O|172799.49|1996-01-02|5-LOW|Clerk#000000951|0|nstructions sleep furiously among |",
        ),
        (
            Table::LineItem,
            "1|1552|93|1|17|24710.35|0.04|0.02|N|O|1996-03-13|1996-02-12|1996-03-22|DELIVER IN PERSON|TRUCK|egular courts above the|",
        ),
    ];

    struct Generated {
        text: Vec<u8>,
        rows: u64,
    }

    fn generate(table: Table, sf: f64) -> Generated {
        let mut text = Vec::new();
        let progress = ProgressBar::hidden();
        let mut sink = CopySink {
            writer: &mut text,
            progress: &progress,
            buffer: Vec::new(),
            line: String::new(),
            rows: 0,
        };
        table.generate(sf, &mut sink).unwrap();
        let rows = sink.rows;
        assert_eq!(progress.position(), rows);
        Generated { text, rows }
    }

    #[test]
    fn copy_lines_are_dbgen_rows_without_the_last_bar() {
        for (table, row) in FIRST_ROWS {
            let generated = generate(table, 0.01);
            let first = generated.text.split(|&b| b == b'\n').next().unwrap();
            let expected = row.strip_suffix('|').unwrap();
            assert_eq!(
                std::str::from_utf8(first).unwrap(),
                expected,
                "{}",
                table.name()
            );
            assert_eq!(generated.text.last(), Some(&b'\n'));
        }
    }

    #[test]
    fn tables_have_the_rows_of_the_specification() {
        for table in Table::ALL {
            let generated = generate(table, 0.01);
            assert_eq!(
                Some(generated.rows as i64),
                table.expected_rows(0.01),
                "{}",
                table.name()
            );
            let lines = generated.text.iter().filter(|&&b| b == b'\n').count();
            assert_eq!(lines as u64, generated.rows);
        }
        assert_eq!(Table::LineItem.expected_rows(1.0), Some(6_001_215));
        assert_eq!(Table::Orders.expected_rows(10.0), Some(15_000_000));
        assert_eq!(Table::PartSupp.expected_rows(1.0), Some(800_000));
        assert_eq!(Table::LineItem.expected_rows(0.5), None);
    }

    #[test]
    fn reserved_characters_are_escaped() {
        let mut buffer = Vec::new();
        push_copy_line(&mut buffer, "1|a\\b|c\rd\ne|");
        assert_eq!(buffer, b"1|a\\\\b|c\\rd\\ne\n");
        buffer.clear();
        push_copy_line(&mut buffer, "no bar");
        assert_eq!(buffer, b"no bar\n");
    }

    #[test]
    fn sizes_read_like_postgres() {
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(10 * 1024), "10 kB");
        assert_eq!(human_size(1536 * 1024 * 1024), "1536 MB");
        assert_eq!(human_size(15 * 1024 * 1024 * 1024), "15 GB");
    }
}
