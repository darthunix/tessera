//! A query as a tree, and its SQL.
//!
//! The generator builds trees of known types, so that every query parses
//! and its operators exist; whether it runs (division by zero, an
//! overflow) is the query's business, and both modes must agree on it.
//! Every operation is written with its parentheses, so the text never
//! depends on precedence.

use std::collections::BTreeSet;
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
    /// `arg COLLATE "collation"`, a text expression under a collation.
    Collate {
        arg: Box<Expr>,
        collation: &'static str,
    },
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
    /// An aggregate: `name([DISTINCT] arg)`, `count(*)` without one;
    /// `FILTER (WHERE true)` after it when filtered, which keeps the core
    /// from planning min and max as a subquery with LIMIT 1.
    Aggregate {
        name: &'static str,
        arg: Option<Box<Expr>>,
        distinct: bool,
        filtered: bool,
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

    /// The tables the expression reads, by their place in the FROM list;
    /// `None` with an aggregate or a subquery in it, which a probe of one
    /// table cannot hold.
    fn tables(&self) -> Option<BTreeSet<usize>> {
        let mut tables = BTreeSet::new();
        let mut parts: Vec<&Expr> = vec![self];
        while let Some(expr) = parts.pop() {
            match expr {
                Expr::Column { table, .. } => {
                    tables.insert(*table);
                }
                Expr::Literal(_) => {}
                Expr::Aggregate { .. } | Expr::Exists { .. } => return None,
                _ => parts.extend(expr.children()),
            }
        }
        Some(tables)
    }

    /// The expressions directly under this one.
    fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Column { .. } | Expr::Literal(_) => Vec::new(),
            Expr::Prefix { arg, .. }
            | Expr::IsNull { arg, .. }
            | Expr::Cast { arg, .. }
            | Expr::Collate { arg, .. }
            | Expr::Like { arg, .. } => vec![arg],
            Expr::Binary { left, right, .. } => vec![left, right],
            Expr::Call { args, .. } => args.iter().collect(),
            Expr::Case {
                when,
                then,
                otherwise,
            } => vec![when, then, otherwise],
            Expr::Between { arg, low, high, .. } => vec![arg, low, high],
            Expr::InList { arg, list, .. } => std::iter::once(&**arg).chain(list).collect(),
            Expr::Exists { outer, .. } => vec![outer],
            Expr::Aggregate { arg, .. } => arg.iter().map(|arg| &**arg).collect(),
        }
    }

    /// The largest parts of the expression that read one table, each with
    /// that table's place: what a probe of the table evaluates. A part of
    /// no table is left out: it fails alike in both modes.
    fn single_table_parts(&self, parts: &mut Vec<(usize, Expr)>) {
        match self.tables() {
            Some(tables) if tables.len() == 1 => {
                parts.extend(tables.first().map(|&table| (table, self.clone())));
            }
            Some(tables) if tables.is_empty() => {}
            _ => {
                for child in self.children() {
                    child.single_table_parts(parts);
                }
            }
        }
    }

    /// The parts of a condition joined by AND at its top.
    fn conjuncts(&self) -> Vec<&Expr> {
        match self {
            Expr::Binary {
                op: "AND",
                left,
                right,
            } => {
                let mut conjuncts = left.conjuncts();
                conjuncts.extend(right.conjuncts());
                conjuncts
            }
            _ => vec![self],
        }
    }

    /// Gives every aggregate in the expression its `FILTER (WHERE true)`.
    fn filter_aggregates(&mut self) {
        match self {
            Expr::Column { .. } | Expr::Literal(_) | Expr::Like { .. } => {}
            Expr::Prefix { arg, .. }
            | Expr::IsNull { arg, .. }
            | Expr::Cast { arg, .. }
            | Expr::Collate { arg, .. } => {
                arg.filter_aggregates();
            }
            Expr::Binary { left, right, .. } => {
                left.filter_aggregates();
                right.filter_aggregates();
            }
            Expr::Call { args, .. } => args.iter_mut().for_each(Expr::filter_aggregates),
            Expr::Case {
                when,
                then,
                otherwise,
            } => {
                when.filter_aggregates();
                then.filter_aggregates();
                otherwise.filter_aggregates();
            }
            Expr::Between { arg, low, high, .. } => {
                arg.filter_aggregates();
                low.filter_aggregates();
                high.filter_aggregates();
            }
            Expr::InList { arg, list, .. } => {
                arg.filter_aggregates();
                list.iter_mut().for_each(Expr::filter_aggregates);
            }
            Expr::Exists { outer, filter, .. } => {
                outer.filter_aggregates();
                if let Some(filter) = filter {
                    filter.filter_aggregates();
                }
            }
            Expr::Aggregate { filtered, .. } => *filtered = true,
        }
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
            Expr::Collate { arg, collation } => {
                out.push('(');
                arg.write(out, depth);
                let _ = write!(out, " COLLATE \"{collation}\")");
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
                filtered,
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
                if *filtered {
                    out.push_str(" FILTER (WHERE true)");
                }
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
    /// then the same multiset in both modes, ties being equal rows. Values
    /// equal but written differently tie too, so the generator gives such
    /// a query its representatives of equal values as outputs.
    pub limit: Option<u32>,
    /// `ORDER BY` every output column without a limit: the rows come in
    /// one order in both modes, compared in it, as under a limit.
    pub ordered: bool,
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
        if self.ordered || self.limit.is_some() {
            out.push_str(" ORDER BY ");
            for index in 0..self.items.len() {
                if index > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{}", index + 1);
            }
        }
        if let Some(limit) = self.limit {
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
    /// Whether the query's rows come in an order of their own, by every
    /// output column, which both modes must give: a single SELECT ordered,
    /// with or without a limit.
    pub fn ordered(&self) -> bool {
        self.set.is_none() && (self.select.ordered || self.select.limit.is_some())
    }

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

impl Query {
    /// The query without what lets a plan stop before the last row: no
    /// LIMIT; no DISTINCT, which over constants the core plans as a LIMIT
    /// 1; every aggregate filtered, so that min and max are no subquery
    /// with LIMIT 1. Its rows are not the query's; it tells whether
    /// evaluating every row raises an error.
    pub fn relaxed(&self) -> Query {
        let relax = |select: &Select| {
            let mut select = Select {
                distinct: false,
                limit: None,
                ordered: false,
                ..select.clone()
            };
            select.items.iter_mut().for_each(Expr::filter_aggregates);
            if let Some(having) = select.grouping.as_mut().and_then(|g| g.having.as_mut()) {
                having.filter_aggregates();
            }
            select
        };
        Query {
            select: relax(&self.select),
            set: self.set.as_ref().map(|(op, other)| (*op, relax(other))),
            settings: self.settings,
        }
    }
}

impl Select {
    /// Each table of the query with the largest parts of its expressions
    /// that read it alone: outputs, aggregates' arguments, join keys and
    /// conditions, the WHERE condition, grouping keys and HAVING, and the
    /// condition of a subquery over its own table. With them, the parts of
    /// the WHERE condition joined by AND that read the table alone: a row
    /// that fails one reaches no output, in any plan.
    fn probes(&self) -> Vec<Probe> {
        let tables: Vec<Table> = std::iter::once(self.from)
            .chain(self.joins.iter().map(|join| join.table))
            .collect();
        let exprs: Vec<&Expr> = self
            .items
            .iter()
            .chain(
                self.joins
                    .iter()
                    .flat_map(|join| join.keys.iter().map(|(_, other)| other).chain(&join.extra)),
            )
            .chain(&self.filter)
            .chain(
                self.grouping
                    .iter()
                    .flat_map(|g| g.keys.iter().chain(&g.having)),
            )
            .collect();
        let mut parts: Vec<(Table, usize, Expr)> = Vec::new();
        for expr in &exprs {
            let mut found = Vec::new();
            expr.single_table_parts(&mut found);
            parts.extend(
                found
                    .into_iter()
                    .filter_map(|(at, part)| tables.get(at).map(|table| (*table, at, part))),
            );
        }
        // A subquery's condition reads its own table, at the place after
        // the FROM list.
        let place = tables.len();
        let mut pending = exprs;
        while let Some(expr) = pending.pop() {
            if let Expr::Exists {
                table,
                filter: Some(filter),
                ..
            } = expr
            {
                let mut found = Vec::new();
                filter.single_table_parts(&mut found);
                parts.extend(
                    found
                        .into_iter()
                        .filter(|(at, _)| *at == place)
                        .map(|(at, part)| (*table, at, part)),
                );
            }
            pending.extend(expr.children());
        }
        let mut probes: Vec<Probe> = Vec::new();
        for (table, at, part) in parts {
            match probes
                .iter_mut()
                .find(|probe| probe.tables == [(at, table)])
            {
                Some(probe) if probe.parts.contains(&part) => {}
                Some(probe) => probe.parts.push(part),
                None => probes.push(Probe {
                    tables: vec![(at, table)],
                    parts: vec![part],
                    filter: Vec::new(),
                }),
            }
        }
        // A constant one, as false, holds for the rows of every table, the
        // subquery's too.
        let conjuncts = self.filter.iter().flat_map(Expr::conjuncts);
        for conjunct in conjuncts {
            let Some(read) = conjunct.tables() else {
                continue;
            };
            for probe in &mut probes {
                let at = probe.tables[0].0;
                if read.is_empty() || (at < place && read.iter().all(|&table| table == at)) {
                    probe.filter.push(conjunct.clone());
                }
            }
        }
        probes
    }

    /// The conditions of the query joined by AND at their tops, of the
    /// WHERE condition and of each join, a join's keys as equalities; with
    /// each, whether it can raise an error over several tables of its own:
    /// a key whose expression reads one table only compares two values.
    fn conditions(&self) -> Vec<(Expr, bool)> {
        let mut conditions: Vec<(Expr, bool)> = self
            .filter
            .iter()
            .flat_map(Expr::conjuncts)
            .map(|conjunct| (conjunct.clone(), true))
            .collect();
        for (index, join) in self.joins.iter().enumerate() {
            for (column, other) in &join.keys {
                let key = Expr::Binary {
                    op: "=",
                    left: Box::new(Expr::Column {
                        table: index + 1,
                        name: column,
                    }),
                    right: Box::new(other.clone()),
                };
                let joint = other.tables().is_some_and(|tables| tables.len() > 1);
                conditions.push((key, joint));
            }
            conditions.extend(
                join.extra
                    .iter()
                    .flat_map(Expr::conjuncts)
                    .map(|conjunct| (conjunct.clone(), true)),
            );
        }
        conditions
    }

    /// Each condition over several tables, over those tables joined by the
    /// query's other conditions that read them alone. Where every join is
    /// inner, a plan may test the condition as soon as it has joined those
    /// tables, before the joins with the others: the core does when its
    /// join order puts them together first. The rows that fail one of the
    /// other conditions it tests first are never tested. An outer join
    /// keeps the rows of its preserved side that fail its conditions, and
    /// lets no plan join its nullable side before the tables it is joined
    /// to: these probes stand for no query with one. A side of an equality
    /// between expressions over two sets of tables is probed by itself too:
    /// a hash or merge join on the equality computes each side over the
    /// join of its own tables, before the other's are joined.
    fn joint_probes(&self) -> Vec<Probe> {
        let tables: Vec<Table> = std::iter::once(self.from)
            .chain(self.joins.iter().map(|join| join.table))
            .collect();
        let conditions = self.conditions();
        let mut probes = Vec::new();
        for (index, (condition, joint)) in conditions.iter().enumerate() {
            if !*joint {
                continue;
            }
            let mut parts = vec![condition];
            if let Expr::Binary {
                op: "=",
                left,
                right,
            } = condition
                && let (Some(on_left), Some(on_right)) = (left.tables(), right.tables())
                && on_left.is_disjoint(&on_right)
            {
                parts.extend([&**left, &**right]);
            }
            for part in parts {
                let Some(read) = part.tables().filter(|read| read.len() > 1) else {
                    continue;
                };
                // Constant conditions too: false joins no rows.
                let filter = conditions
                    .iter()
                    .enumerate()
                    .filter(|(other, (expr, _))| {
                        *other != index && expr.tables().is_some_and(|by| by.is_subset(&read))
                    })
                    .map(|(_, (expr, _))| expr.clone())
                    .collect();
                probes.push(Probe {
                    tables: read
                        .iter()
                        .filter_map(|&at| tables.get(at).map(|table| (at, *table)))
                        .collect(),
                    parts: vec![part.clone()],
                    filter,
                });
            }
        }
        probes
    }
}

/// A probe: parts of the query's expressions over the rows of its tables
/// that pass their own conditions, each table at its place in the query's
/// FROM list.
struct Probe {
    tables: Vec<(usize, Table)>,
    parts: Vec<Expr>,
    filter: Vec<Expr>,
}

impl Query {
    /// Queries that evaluate, for every row of each table that passes the
    /// table's own conditions, the largest parts of the query's
    /// expressions that read that table alone. One raises an error when a
    /// row of the data does, whatever rows a plan of the query would skip.
    pub fn probes(&self) -> Vec<String> {
        self.selects()
            .flat_map(Select::probes)
            .map(|probe| probe.sql())
            .collect()
    }

    /// Queries that evaluate each condition over several tables for every
    /// row of their join by the query's other conditions over them alone
    /// (Select::joint_probes): for a query whose joins are all inner, as
    /// the planner leaves them.
    pub fn joint_probes(&self) -> Vec<String> {
        self.selects()
            .flat_map(Select::joint_probes)
            .map(|probe| probe.sql())
            .collect()
    }

    /// Each SELECT of the query as a query of its own, under its settings,
    /// with the count of its joins.
    pub fn branches(&self) -> Vec<(Query, usize)> {
        self.selects()
            .map(|select| {
                let branch = Query {
                    select: select.clone(),
                    set: None,
                    settings: self.settings,
                };
                (branch, select.joins.len())
            })
            .collect()
    }

    fn selects(&self) -> impl Iterator<Item = &Select> {
        std::iter::once(&self.select).chain(self.set.iter().map(|(_, other)| other))
    }
}

impl Probe {
    /// The probe's query. The rows go to text, so that nothing is left out
    /// of the plan, and only their lengths add up, so that a join of two
    /// large tables keeps no array of its rows.
    fn sql(&self) -> String {
        let depth = self.tables.iter().map(|(at, _)| at + 1).max().unwrap_or(1);
        let mut out = String::from("SELECT sum(length(q::text)) FROM (SELECT ");
        for (index, expr) in self.parts.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            expr.write(&mut out, depth);
            let _ = write!(out, " AS p{index}");
        }
        for (index, (at, table)) in self.tables.iter().enumerate() {
            out.push_str(if index == 0 { " FROM " } else { ", " });
            let _ = write!(out, "{} AS t{at}", table.name);
        }
        for (index, conjunct) in self.filter.iter().enumerate() {
            out.push_str(if index == 0 { " WHERE " } else { " AND " });
            conjunct.write(&mut out, depth);
        }
        out.push_str(") AS q");
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
    use crate::schema::{DIM, FACT, TINY};

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
                ordered: false,
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
    fn a_collation_and_an_order_without_a_limit() {
        let query = Query {
            select: Select {
                distinct: false,
                items: vec![Expr::Collate {
                    arg: Box::new(column(0, "t")),
                    collation: "nd",
                }],
                from: TINY,
                joins: Vec::new(),
                filter: None,
                grouping: None,
                limit: None,
                ordered: true,
            },
            set: None,
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        assert_eq!(
            query.sql(),
            "SELECT (t0.t COLLATE \"nd\") AS c0 FROM tiny AS t0 ORDER BY 1"
        );
        assert!(query.ordered());
        assert!(!query.relaxed().ordered());
    }

    #[test]
    fn a_relaxed_query_has_no_limit_or_distinct() {
        let select = Select {
            distinct: true,
            items: vec![column(0, "a")],
            from: FACT,
            joins: Vec::new(),
            filter: None,
            grouping: None,
            limit: Some(3),
            ordered: false,
        };
        let query = Query {
            select: select.clone(),
            set: Some((SetOp::Intersect, select)),
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        assert_eq!(
            query.relaxed().sql(),
            "(SELECT t0.a AS c0 FROM fact AS t0) INTERSECT (SELECT t0.a AS c0 FROM fact AS t0)"
        );
        let mut aggregated = query.select.clone();
        aggregated.items = vec![Expr::Call {
            name: "trim_scale",
            args: vec![Expr::Aggregate {
                name: "max",
                arg: Some(Box::new(column(0, "m"))),
                distinct: false,
                filtered: false,
            }],
        }];
        let query = Query {
            select: aggregated,
            set: None,
            ..query
        };
        assert_eq!(
            query.relaxed().sql(),
            "SELECT trim_scale(max(t0.m) FILTER (WHERE true)) AS c0 FROM fact AS t0"
        );
    }

    #[test]
    fn a_probe_reads_each_table_through_its_own_parts_of_the_expressions() {
        let query = Query {
            select: Select {
                distinct: false,
                items: vec![
                    Expr::Binary {
                        op: "*",
                        left: Box::new(column(0, "a")),
                        right: Box::new(column(1, "a")),
                    },
                    Expr::Aggregate {
                        name: "sum",
                        arg: Some(Box::new(Expr::Cast {
                            arg: Box::new(column(1, "c")),
                            ty: Ty::Int4,
                        })),
                        distinct: false,
                        filtered: false,
                    },
                ],
                from: FACT,
                joins: vec![Join {
                    kind: JoinKind::Inner,
                    table: DIM,
                    keys: vec![(
                        "a",
                        Expr::Binary {
                            op: "+",
                            left: Box::new(column(0, "a")),
                            right: Box::new(Expr::literal("1::int4")),
                        },
                    )],
                    extra: None,
                }],
                filter: Some(Expr::Binary {
                    op: "AND",
                    left: Box::new(Expr::IsNull {
                        arg: Box::new(column(0, "b")),
                        negated: true,
                    }),
                    right: Box::new(Expr::Exists {
                        table: DIM,
                        inner: "id",
                        outer: Box::new(column(1, "id")),
                        filter: Some(Box::new(Expr::IsNull {
                            arg: Box::new(Expr::Binary {
                                op: "/",
                                left: Box::new(Expr::literal("1::int4")),
                                right: Box::new(column(2, "a")),
                            }),
                            negated: false,
                        })),
                        negated: false,
                    }),
                }),
                grouping: Some(Grouping {
                    keys: vec![column(0, "a")],
                    having: None,
                }),
                limit: None,
                ordered: false,
            },
            set: None,
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        assert_eq!(
            query.probes(),
            vec![
                "SELECT sum(length(q::text)) FROM (SELECT t0.a AS p0, (t0.a + 1::int4) AS p1, \
                 (t0.b IS NOT NULL) AS p2 FROM fact AS t0 WHERE (t0.b IS NOT NULL)) AS q",
                "SELECT sum(length(q::text)) FROM (SELECT t1.a AS p0, (t1.c)::int4 AS p1, \
                 t1.id AS p2 FROM dim AS t1) AS q",
                "SELECT sum(length(q::text)) FROM (SELECT ((1::int4 / t2.a) IS NULL) AS p0 \
                 FROM dim AS t2) AS q",
            ]
        );
    }

    /// The finding of seed 20261127: the core tested the WHERE condition
    /// over t0 and t2 at their join, which Tessera's costs put first, and
    /// divided by a zero of t0.c that its own order never reaches. The
    /// probe of the condition joins the two by the key between them alone;
    /// keys of a column and of an expression over one table make none.
    #[test]
    fn a_condition_over_two_tables_is_probed_over_their_join_by_the_others() {
        let query = Query {
            select: Select {
                distinct: false,
                items: vec![column(0, "id")],
                from: DIM,
                joins: vec![
                    Join {
                        kind: JoinKind::Inner,
                        table: FACT,
                        keys: vec![("a", column(0, "id"))],
                        extra: None,
                    },
                    Join {
                        kind: JoinKind::Left,
                        table: FACT,
                        keys: vec![
                            (
                                "t",
                                Expr::Collate {
                                    arg: Box::new(column(0, "t")),
                                    collation: "C",
                                },
                            ),
                            ("id", column(1, "c")),
                        ],
                        extra: Some(Expr::Binary {
                            op: "<=",
                            left: Box::new(column(1, "c")),
                            right: Box::new(column(1, "s")),
                        }),
                    },
                ],
                filter: Some(Expr::InList {
                    arg: Box::new(Expr::Binary {
                        op: "/",
                        left: Box::new(column(2, "c")),
                        right: Box::new(column(0, "c")),
                    }),
                    list: vec![Expr::literal("'-14'::int8"), Expr::literal("NULL::int8")],
                    negated: true,
                }),
                grouping: None,
                limit: None,
                ordered: false,
            },
            set: None,
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        assert!(
            query
                .probes()
                .iter()
                .all(|probe| !probe.contains(", fact AS"))
        );
        assert_eq!(
            query.joint_probes(),
            vec![
                "SELECT sum(length(q::text)) FROM (SELECT ((t2.c / t0.c) NOT IN \
                 ('-14'::int8, NULL::int8)) AS p0 FROM dim AS t0, fact AS t2 \
                 WHERE (t2.t = (t0.t COLLATE \"C\"))) AS q"
            ]
        );
    }

    /// A hash join on t2.a = (t0.c / t1.c) computes the division over the
    /// join of t0 and t1, before t2 drops a pair: that side has a probe of
    /// its own. A constant condition stays in the probes it holds for.
    #[test]
    fn a_side_of_an_equality_over_two_tables_is_probed_by_itself() {
        let query = Query {
            select: Select {
                distinct: false,
                items: vec![column(0, "id")],
                from: DIM,
                joins: vec![
                    Join {
                        kind: JoinKind::Inner,
                        table: FACT,
                        keys: vec![("id", column(0, "id"))],
                        extra: Some(Expr::literal("false")),
                    },
                    Join {
                        kind: JoinKind::Inner,
                        table: TINY,
                        keys: vec![("id", column(0, "id"))],
                        extra: None,
                    },
                ],
                filter: Some(Expr::Binary {
                    op: "=",
                    left: Box::new(column(2, "a")),
                    right: Box::new(Expr::Binary {
                        op: "/",
                        left: Box::new(column(0, "c")),
                        right: Box::new(column(1, "c")),
                    }),
                }),
                grouping: None,
                limit: None,
                ordered: false,
            },
            set: None,
            settings: Settings {
                workers: 0,
                work_mem: "64kB",
            },
        };
        assert_eq!(
            query.joint_probes(),
            vec![
                "SELECT sum(length(q::text)) FROM (SELECT (t2.a = (t0.c / t1.c)) AS p0 \
                 FROM dim AS t0, fact AS t1, tiny AS t2 \
                 WHERE (t1.id = t0.id) AND false AND (t2.id = t0.id)) AS q",
                "SELECT sum(length(q::text)) FROM (SELECT (t0.c / t1.c) AS p0 \
                 FROM dim AS t0, fact AS t1 WHERE (t1.id = t0.id) AND false) AS q",
            ]
        );
    }

    #[test]
    fn quotes_are_doubled() {
        assert_eq!(quote("it's"), "'it''s'");
    }
}
