//! The tables the queries read and the SQL that fills them from a seed.
//!
//! Three tables of different sizes, and a table of strings, share their
//! key columns' ranges, so that joins find matches, misses and
//! duplicates: `fact` with a few thousand rows, `dim` with two hundred,
//! `tiny` with five, and `words`, of strings, with three thousand,
//! which has a Parquet twin as well (see the oracle). Every column but
//! the key `id` has NULLs, and the values lean to the edges where code
//! breaks: the ends of the integer types, zero and ±1, empty strings,
//! numerics of several scales, dates far from today, NaN and the
//! infinities; strings of characters of two, three and four bytes in
//! UTF-8, a combining accent, and pairs a collation that ignores case
//! takes as equal, as `é` and `É`. The setup also makes the collations
//! the queries name: `lc`, of libc for a language (`en_US.UTF-8`, or
//! `C.UTF-8` where that is missing), and `nd`, ICU's nondeterministic
//! one that ignores case, which needs a server built with ICU. The rows
//! come from PostgreSQL's own `random()` after `setseed`, in one
//! connection, so a seed gives the same data on the same build every
//! time, and a finding's file can carry the SQL that makes it.

use std::fmt::Write;

/// The SQL types the queries use, as the generator tracks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Int2,
    Int4,
    Int8,
    Numeric,
    Float8,
    Text,
    Date,
    Timestamp,
    Bool,
}

impl Ty {
    /// The types of the integer family, which mix in arithmetic and
    /// comparisons.
    pub fn is_integer(self) -> bool {
        matches!(self, Ty::Int2 | Ty::Int4 | Ty::Int8)
    }

    /// The type's name in a cast.
    pub fn sql(self) -> &'static str {
        match self {
            Ty::Int2 => "int2",
            Ty::Int4 => "int4",
            Ty::Int8 => "int8",
            Ty::Numeric => "numeric",
            Ty::Float8 => "float8",
            Ty::Text => "text",
            Ty::Date => "date",
            Ty::Timestamp => "timestamp",
            Ty::Bool => "bool",
        }
    }
}

/// A column of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub ty: Ty,
}

/// A table the queries read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    pub name: &'static str,
    pub columns: &'static [Column],
}

const fn column(name: &'static str, ty: Ty) -> Column {
    Column { name, ty }
}

pub const FACT: Table = Table {
    name: "fact",
    columns: &[
        column("id", Ty::Int4),
        column("a", Ty::Int4),
        column("b", Ty::Int4),
        column("c", Ty::Int8),
        column("s", Ty::Int2),
        column("m", Ty::Numeric),
        column("p", Ty::Numeric),
        column("f", Ty::Float8),
        column("t", Ty::Text),
        column("d", Ty::Date),
        column("ts", Ty::Timestamp),
        column("bo", Ty::Bool),
    ],
};

pub const DIM: Table = Table {
    name: "dim",
    columns: &[
        column("id", Ty::Int4),
        column("a", Ty::Int4),
        column("c", Ty::Int8),
        column("m", Ty::Numeric),
        column("t", Ty::Text),
        column("d", Ty::Date),
    ],
};

pub const TINY: Table = Table {
    name: "tiny",
    columns: &[
        column("id", Ty::Int4),
        column("a", Ty::Int4),
        column("t", Ty::Text),
    ],
};

/// The table of strings: `t` of a few words, which a Parquet writer keeps
/// through a dictionary, and `u` of longer strings, most of them apart.
/// It has a Parquet twin, `words_pq`.
pub const WORDS: Table = Table {
    name: "words",
    columns: &[
        column("id", Ty::Int4),
        column("a", Ty::Int4),
        column("t", Ty::Text),
        column("u", Ty::Text),
    ],
};

/// The tables, largest first but for the table of strings.
pub const TABLES: [Table; 4] = [FACT, DIM, TINY, WORDS];

/// The rows of the table of strings.
pub const WORDS_ROWS: u32 = 3000;

/// A word of the table of strings: cases, accents whole and combining, a
/// character of a script of three bytes, one of four, and LIKE's own
/// characters.
const WORD: &str = "(ARRAY['apple', 'Apple', 'APPLE', 'caf\u{e9}', 'CAF\u{c9}', 'cafe\u{301}', \
     'stra\u{df}e', 'STRASSE', 'na\u{ef}ve', '\u{65e5}\u{672c}', '\u{65e5}\u{672c}\u{8a9e}', \
     '\u{1f600}', '\u{20ac}uro', 'zzz', 'a%b', '_'])[1 + floor(random() * 16)::int]";

/// A value of a column: NULL with `nulls`, an edge with `edges`, otherwise
/// `common`; each expression calls `random()` afresh, left to right.
fn value(nulls: f64, edges: &str, edge_rate: f64, common: &str) -> String {
    format!(
        "CASE WHEN random() < {nulls} THEN NULL \
         WHEN random() < {edge_rate} THEN ({edges})[1 + floor(random() * cardinality({edges}))::int] \
         ELSE {common} END"
    )
}

/// A small integer from `-range` to `range`.
fn small(range: i64) -> String {
    format!("(floor(random() * {}) - {range})", 2 * range + 1)
}

const INT4_EDGES: &str =
    "ARRAY[-2147483648, -2147483647, -1, 0, 1, 2147483646, 2147483647]::int4[]";
const INT8_EDGES: &str = "ARRAY[-9223372036854775808, -9223372036854775807, -2147483648, \
     2147483647, 4294967296, -1, 0, 1, 9223372036854775807]::int8[]";
const INT2_EDGES: &str = "ARRAY[-32768, -32767, -1, 0, 1, 32767]::int2[]";
const NUMERIC_EDGES: &str = "ARRAY[0, -0.01, 0.01, 9999999999.99, -9999999999.99, \
     999999999999999999, 0.000001, 1e20, -1e-20]::numeric[]";
const FLOAT8_EDGES: &str =
    "ARRAY['NaN', 'Infinity', '-Infinity', '0', '-0', '1e308', '-1e-308']::float8[]";
const TEXT_EDGES: &str = "ARRAY['', ' ', '%', '_', 'a%b', 'zzzz', '\u{e9}', 'e\u{301}', \
     '\u{c9}', '\u{df}', 'SS', '\u{130}', '\u{20ac}', '\u{1f600}']::text[]";
const DATE_EDGES: &str =
    "ARRAY['4713-11-24 BC', '0001-01-01', '1999-12-31', '2000-01-01', '9999-12-31']::date[]";
const TIMESTAMP_EDGES: &str = "ARRAY['4713-11-24 00:00:00 BC', '2000-01-01 00:00:00', \
     '1999-12-31 23:59:59.999999', '9999-12-31 23:59:59']::timestamp[]";

/// A short string of a few characters, with spaces, `%`, `_`, capitals and
/// characters of two bytes among them, as LIKE patterns and comparisons
/// need.
const TEXT_COMMON: &str = "translate(substr(md5(random()::text), 1, floor(random() * 7)::int), \
     '012345', ' %_A\u{e9}\u{df}')";

fn int4(range: i64) -> String {
    value(0.1, INT4_EDGES, 0.05, &format!("{}::int4", small(range)))
}

fn int8(range: i64) -> String {
    value(0.1, INT8_EDGES, 0.05, &format!("{}::int8", small(range)))
}

fn numeric(scale_varies: bool) -> String {
    let scale = if scale_varies {
        "floor(random() * 7)::int"
    } else {
        "2"
    };
    value(
        0.1,
        NUMERIC_EDGES,
        0.05,
        &format!("round((random() * 20000 - 10000)::numeric, {scale})"),
    )
}

fn text() -> String {
    value(0.1, TEXT_EDGES, 0.1, TEXT_COMMON)
}

fn date() -> String {
    value(
        0.1,
        DATE_EDGES,
        0.03,
        "date '2000-01-01' + (floor(random() * 4001) - 2000)::int",
    )
}

/// The SQL that drops and creates the tables with the data of `seed` and
/// `rows` rows of `fact`, then analyzes them. One connection must run it
/// in one piece: `setseed` holds for the session.
pub fn setup(seed: u64, rows: u32) -> String {
    // setseed takes a fraction in [-1, 1].
    let fraction = (seed % 2_000_001) as f64 / 1_000_000.0 - 1.0;
    let mut sql = String::new();
    // A cluster an older version of the tool made has another encoding,
    // under which the queries' strings and collations fail in both modes.
    let _ = writeln!(
        sql,
        "DO $$ BEGIN IF current_setting('server_encoding') <> 'UTF8' THEN \
         RAISE EXCEPTION 'the cluster is not UTF8: remove target/bench-runs/pgdata-crosscheck'; \
         END IF; END $$;"
    );
    let _ = writeln!(sql, "DROP TABLE IF EXISTS fact, dim, tiny, words;");
    let _ = writeln!(
        sql,
        "DROP COLLATION IF EXISTS lc; DROP COLLATION IF EXISTS nd;"
    );
    let _ = writeln!(
        sql,
        "DO $$ BEGIN CREATE COLLATION lc (provider = libc, locale = 'en_US.UTF-8'); \
         EXCEPTION WHEN OTHERS THEN CREATE COLLATION lc (provider = libc, locale = 'C.UTF-8'); \
         END $$;"
    );
    let _ = writeln!(
        sql,
        "CREATE COLLATION nd (provider = icu, locale = 'und-u-ks-level2', deterministic = false);"
    );
    // The pairs of the data that `nd` is there for: equal, other bytes.
    let _ = writeln!(
        sql,
        "DO $$ BEGIN IF NOT ('\u{e9}' = '\u{c9}' COLLATE nd AND '\u{e9}' = 'e\u{301}' COLLATE nd) \
         THEN RAISE EXCEPTION 'nd does not take the pairs of the data as equal'; END IF; END $$;"
    );
    let _ = writeln!(sql, "SELECT setseed({fraction});");
    let _ = writeln!(
        sql,
        "CREATE TABLE fact AS SELECT g AS id, {a} AS a, {b} AS b, {c} AS c, {s} AS s, \
         {m} AS m, {p} AS p, {f} AS f, {t} AS t, {d} AS d, {ts} AS ts, \
         CASE WHEN random() < 0.1 THEN NULL ELSE random() < 0.5 END AS bo \
         FROM generate_series(1, {rows}) AS g;",
        a = int4(50),
        b = int4(100_000),
        c = int8(50),
        s = value(0.1, INT2_EDGES, 0.05, &format!("{}::int2", small(1000))),
        m = numeric(false),
        p = numeric(true),
        f = value(
            0.1,
            FLOAT8_EDGES,
            0.05,
            "(floor(random() * 400) - 200)::float8 / 4"
        ),
        t = text(),
        d = date(),
        ts = value(
            0.1,
            TIMESTAMP_EDGES,
            0.03,
            "timestamp '2000-01-01' + random() * interval '4000 days' - interval '2000 days'"
        ),
    );
    let _ = writeln!(
        sql,
        "CREATE TABLE dim AS SELECT g AS id, {a} AS a, {c} AS c, {m} AS m, {t} AS t, \
         {d} AS d FROM generate_series(1, 200) AS g;",
        a = int4(50),
        c = int8(50),
        m = numeric(false),
        t = text(),
        d = date(),
    );
    let _ = writeln!(
        sql,
        "CREATE TABLE tiny AS SELECT g AS id, {a} AS a, {t} AS t \
         FROM generate_series(1, 5) AS g;",
        a = int4(5),
        t = text(),
    );
    let _ = writeln!(
        sql,
        "CREATE TABLE words AS SELECT g AS id, {a} AS a, {t} AS t, {u} AS u \
         FROM generate_series(1, {WORDS_ROWS}) AS g;",
        a = int4(50),
        t = value(0.1, TEXT_EDGES, 0.05, WORD),
        u = value(
            0.1,
            TEXT_EDGES,
            0.05,
            &format!("repeat({WORD}, floor(random() * 4)::int) || {TEXT_COMMON}")
        ),
    );
    // Indexes give the planner index and bitmap paths to choose.
    let _ = writeln!(sql, "CREATE INDEX fact_b ON fact (b);");
    let _ = writeln!(sql, "CREATE INDEX dim_id ON dim (id);");
    let _ = writeln!(sql, "ANALYZE fact, dim, tiny, words;");
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_starts_with_its_key() {
        for table in TABLES {
            assert_eq!(table.columns[0], column("id", Ty::Int4), "{}", table.name);
        }
    }

    #[test]
    fn the_setup_creates_every_column_of_the_tables() {
        let sql = setup(42, 100);
        for table in TABLES {
            for column in table.columns {
                assert!(
                    sql.contains(&format!(" AS {},", column.name))
                        || sql.contains(&format!(" AS {} ", column.name)),
                    "{}.{}",
                    table.name,
                    column.name
                );
            }
        }
        assert!(sql.contains("setseed(-0.999958)"));
    }
}
