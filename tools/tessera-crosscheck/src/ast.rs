//! A query as a tree, and its SQL.
//!
//! The generator builds trees of known types, so that every query parses
//! and its operators exist; whether it runs (division by zero, an
//! overflow) is the query's business, and both modes must agree on it.
//! Every operation is written with its parentheses, so the text never
//! depends on precedence.

use std::fmt::{self, Display, Write};

use crate::schema::{Table, Ty};

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A column of the table at `table` in the query's FROM list.
    Column { table: usize, name: &'static str },
    /// A literal, already written as SQL with its cast.
    Literal(String),
    /// `op arg`: `-`, `NOT`.
    Prefix { op: &'static str, arg: Box<Expr> },
    /// `left op right`.
    Binary {
        op: &'static str,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// A function call.
    Call { name: &'static str, args: Vec<Expr> },
    /// `CASE WHEN when THEN then ELSE otherwise END`.
    Case {
        when: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    /// `arg IS [NOT] NULL`.
    IsNull { arg: Box<Expr>, negated: bool },
    /// `arg [NOT] BETWEEN low AND high`.
    Between {
        arg: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    /// `arg [NOT] IN (list)`.
    InList {
        arg: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `arg [NOT] LIKE 'pattern'`.
    Like {
        arg: Box<Expr>,
        pattern: String,
        negated: bool,
    },
    /// `arg::ty`.
    Cast { arg: Box<Expr>, ty: Ty },
    /// `[NOT] EXISTS (SELECT 1 FROM table AS sN WHERE sN.inner = outer AND filter)`,
    /// a semi or anti join; `filter` refers to the inner table as the
    /// table after the outer query's FROM list.
    Exists {
        table: Table,
        inner: &'static str,
        outer: Box<Expr>,
        filter: Option<Box<Expr>>,
        negated: bool,
    },
    /// An aggregate: `name([DISTINCT] arg)`, `count(*)` without one.
    Aggregate {
        name: &'static str,
        arg: Option<Box<Expr>>,
        distinct: bool,
    },
}

/// A text literal, quoted.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl Expr {
    pub fn literal(sql: impl Into<String>) -> Expr {
        Expr::Literal(sql.into())
    }

    /// Writes the expression; `depth` is the number of FROM entries of the
    /// enclosing queries, for the aliases of a subquery's table.
    fn write(&self, out: &mut String, depth: usize) {
        match self {
            Expr::Column { table, name } => {
                let _ = write!(out, "t{table}.{name}");
            }
            Expr::Literal(sql) => out.push_str(sql),
            Expr::Prefix { op, arg } => {
                let _ = write!(out, "({op} ");
                arg.write(out, depth);
                out.push(')');
            }
            Expr::Binary { op, left, right } => {
                out.push('(');
                left.write(out, depth);
                let _ = write!(out, " {op} ");
                right.write(out, depth);
                out.push(')');
            }
            Expr::Call { name, args } => {
                let _ = write!(out, "{name}(");
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    arg.write(out, depth);
                }
                out.push(')');
            }
            Expr::Case {
                when,
                then,
                otherwise,
            } => {
                out.push_str("(CASE WHEN ");
                when.write(out, depth);
                out.push_str(" THEN ");
                then.write(out, depth);
                out.push_str(" ELSE ");
                otherwise.write(out, depth);
                out.push_str(" END)");
            }
            Expr::IsNull { arg, negated } => {
                out.push('(');
                arg.write(out, depth);
                out.push_str(if *negated {
                    " IS NOT NULL)"
                } else {
                    " IS NULL)"
                });
            }
            Expr::Between {
                arg,
                low,
                high,
                negated,
            } => {
                out.push('(');
                arg.write(out, depth);
                out.push_str(if *negated {
                    " NOT BETWEEN "
                } else {
                    " BETWEEN "
                });
                low.write(out, depth);
                out.push_str(" AND ");
                high.write(out, depth);
                out.push(')');
            }
            Expr::InList { arg, list, negated } => {
                out.push('(');
                arg.write(out, depth);
                out.push_str(if *negated { " NOT IN (" } else { " IN (" });
                for (index, item) in list.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    item.write(out, depth);
                }
                out.push_str("))");
            }
            Expr::Like {
                arg,
                pattern,
                negated,
            } => {
                out.push('(');
                arg.write(out, depth);
                out.push_str(if *negated { " NOT LIKE " } else { " LIKE " });
                out.push_str(&quote(pattern));
                out.push(')');
            }
            Expr::Cast { arg, ty } => {
                out.push('(');
                arg.write(out, depth);
                let _ = write!(out, ")::{}", ty.sql());
            }
            Expr::Exists {
                table,
                inner,
                outer,
                filter,
                negated,
            } => {
                let alias = format!("t{depth}");
                out.push_str(if *negated {
                    "(NOT EXISTS ("
                } else {
                    "(EXISTS ("
                });
                let _ = write!(
                    out,
                    "SELECT 1 FROM {} AS {alias} WHERE {alias}.{inner} = ",
                    table.name
                );
                outer.write(out, depth);
                if let Some(filter) = filter {
                    out.push_str(" AND ");
                    filter.write(out, depth + 1);
                }
                out.push_str("))");
            }
            Expr::Aggregate {
                name,
                arg,
                distinct,
            } => {
                let _ = write!(out, "{name}(");
                match arg {
                    None => out.push('*'),
                    Some(arg) => {
                        if *distinct {
                            out.push_str("DISTINCT ");
                        }
                        arg.write(out, depth);
                    }
                }
                out.push(')');
            }
        }
    }
}

/// How a table joins the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

impl JoinKind {
    fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
        }
    }
}

/// A table joined to the ones before it: on equal keys, and an extra
/// condition when there is one.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: Table,
    /// Pairs of a column of this table and an expression over the tables
    /// before it.
    pub keys: Vec<(&'static str, Expr)>,
    pub extra: Option<Expr>,
}

/// The aggregation of a query: its keys and the condition on the groups.
#[derive(Clone, Debug, PartialEq)]
pub struct Grouping {
    pub keys: Vec<Expr>,
    pub having: Option<Expr>,
}

/// A set operation between two queries of the same row type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    Union,
    UnionAll,
    Intersect,
    IntersectAll,
    Except,
    ExceptAll,
}

impl SetOp {
    fn sql(self) -> &'static str {
        match self {
            SetOp::Union => "UNION",
            SetOp::UnionAll => "UNION ALL",
            SetOp::Intersect => "INTERSECT",
            SetOp::IntersectAll => "INTERSECT ALL",
            SetOp::Except => "EXCEPT",
            SetOp::ExceptAll => "EXCEPT ALL",
        }
    }
}

/// A SELECT.
#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<Expr>,
    pub from: Table,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub grouping: Option<Grouping>,
    /// `ORDER BY` every output column, then `LIMIT`: the rows kept are
    /// then the same multiset in both modes, ties being equal rows.
    pub limit: Option<u32>,
}

impl Select {
    fn write(&self, out: &mut String) {
        out.push_str("SELECT ");
        if self.distinct {
            out.push_str("DISTINCT ");
        }
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            item.write(out, 1 + self.joins.len());
            let _ = write!(out, " AS c{index}");
        }
        let _ = write!(out, " FROM {} AS t0", self.from.name);
        for (index, join) in self.joins.iter().enumerate() {
            let alias = format!("t{}", index + 1);
            let _ = write!(
                out,
                " {} {} AS {alias} ON ",
                join.kind.sql(),
                join.table.name
            );
            for (key, (column, other)) in join.keys.iter().enumerate() {
                if key > 0 {
                    out.push_str(" AND ");
                }
                let _ = write!(out, "{alias}.{column} = ");
                other.write(out, index + 1);
            }
            if let Some(extra) = &join.extra {
                out.push_str(" AND ");
                extra.write(out, index + 2);
            }
        }
        let depth = 1 + self.joins.len();
        if let Some(filter) = &self.filter {
            out.push_str(" WHERE ");
            filter.write(out, depth);
        }
        if let Some(grouping) = &self.grouping {
            if !grouping.keys.is_empty() {
                out.push_str(" GROUP BY ");
                for (index, key) in grouping.keys.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    key.write(out, depth);
                }
            }
            if let Some(having) = &grouping.having {
                out.push_str(" HAVING ");
                having.write(out, depth);
            }
        }
        if let Some(limit) = self.limit {
            out.push_str(" ORDER BY ");
            for index in 0..self.items.len() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{}", index + 1);
            }
            let _ = write!(out, " LIMIT {limit}");
        }
    }
}

/// The settings a query runs under, in both modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// max_parallel_workers_per_gather; with workers, parallel plans cost
    /// nothing extra, so the planner takes them where it can.
    pub workers: u8,
    pub work_mem: &'static str,
}

impl Settings {
    /// The statements that set a session up for these settings.
    pub fn statements(&self) -> String {
        let parallel = if self.workers > 0 {
            "SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0; \
             SET min_parallel_table_scan_size = 0; SET tessera.scan_parallel_setup_cost = 0; \
             SET tessera.scan_worker_page_cost = 0;"
        } else {
            "RESET parallel_setup_cost; RESET parallel_tuple_cost; \
             RESET min_parallel_table_scan_size; RESET tessera.scan_parallel_setup_cost; \
             RESET tessera.scan_worker_page_cost;"
        };
        format!(
            "SET max_parallel_workers_per_gather = {}; SET work_mem = '{}'; {parallel}",
            self.workers, self.work_mem
        )
    }
}

/// A query and the settings it runs under.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub select: Select,
    /// Another query of the same row type and how the two combine.
    pub set: Option<(SetOp, Select)>,
    pub settings: Settings,
}

impl Query {
    /// The query's SQL.
    pub fn sql(&self) -> String {
        let mut out = String::new();
        match &self.set {
            None => self.select.write(&mut out),
            Some((op, other)) => {
                out.push('(');
                self.select.write(&mut out);
                let _ = write!(out, ") {} (", op.sql());
                other.write(&mut out);
                out.push(')');
            }
        }
        out
    }
}

impl Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{};", self.settings.statements(), self.sql())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DIM, FACT};

    fn column(table: usize, name: &'static str) -> Expr {
        Expr::Column { table, name }
    }

    #[test]
    fn a_query_is_written_with_every_parenthesis() {
        let query = Query {
            select: Select {
                distinct: false,
                items: vec![
                    column(0, "a"),
                    Expr::Binary {
                        op: "+",
                        left: Box::new(column(1, "c")),
                        right: Box::new(Expr::literal("1::int8")),
                    },
                ],
                from: FACT,
                joins: vec![Join {
                    kind: JoinKind::Left,
                    table: DIM,
                    keys: vec![("a", column(0, "a"))],
                    extra: Some(Expr::IsNull {
                        arg: Box::new(column(1, "t")),
                        negated: true,
                    }),
                }],
                filter: Some(Expr::Exists {
                    table: DIM,
                    inner: "id",
                    outer: Box::new(column(0, "b")),
                    filter: Some(Box::new(Expr::Like {
                        arg: Box::new(column(2, "t")),
                        pattern: "a%".into(),
                        negated: false,
                    })),
                    negated: true,
                }),
                grouping: None,
                limit: Some(3),
            },
            set: None,
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        assert_eq!(
            query.sql(),
            "SELECT t0.a AS c0, (t1.c + 1::int8) AS c1 FROM fact AS t0 \
             LEFT JOIN dim AS t1 ON t1.a = t0.a AND (t1.t IS NOT NULL) \
             WHERE (NOT EXISTS (SELECT 1 FROM dim AS t2 WHERE t2.id = t0.b \
             AND (t2.t LIKE 'a%'))) ORDER BY 1, 2 LIMIT 3"
        );
    }

    #[test]
    fn quotes_are_doubled() {
        assert_eq!(quote("it's"), "'it''s'");
    }
}
