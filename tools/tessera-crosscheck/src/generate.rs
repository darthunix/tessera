//! Strategies that generate queries, for proptest: it draws them from a
//! seed and, when one fails, shrinks it to a small one that still fails.
//!
//! Expressions are built by depth and type: at depth 0 a column of the
//! type in scope or a literal of it, at each next depth those or an
//! operation over expressions of the previous depth. Simpler choices come
//! first in each union, so shrinking moves a failing query towards leaves
//! and away from operations.

use std::collections::HashMap;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::Union;

use crate::ast::{Expr, Grouping, Join, JoinKind, Query, Select, SetOp, Settings, quote};
use crate::schema::{Column, TABLES, Table, Ty};

/// Every type an expression may have.
const TYPES: [Ty; 9] = [
    Ty::Int4,
    Ty::Int8,
    Ty::Int2,
    Ty::Numeric,
    Ty::Float8,
    Ty::Text,
    Ty::Date,
    Ty::Timestamp,
    Ty::Bool,
];

/// The depth of the expressions of a query.
const DEPTH: u32 = 2;

/// The columns a query's expressions may read: each with the position of
/// its table in the FROM list.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub columns: Vec<(usize, Column)>,
}

impl Scope {
    pub fn of(tables: &[Table]) -> Scope {
        Scope {
            columns: tables
                .iter()
                .enumerate()
                .flat_map(|(index, table)| table.columns.iter().map(move |column| (index, *column)))
                .collect(),
        }
    }

    fn of_type(&self, ty: Ty) -> Vec<Expr> {
        self.columns
            .iter()
            .filter(|(_, column)| column.ty == ty)
            .map(|(table, column)| Expr::Column {
                table: *table,
                name: column.name,
            })
            .collect()
    }
}

fn boxed(expr: Expr) -> Box<Expr> {
    Box::new(expr)
}

/// A literal of the type: its edges often, NULL now and then.
pub fn literal(ty: Ty) -> BoxedStrategy<Expr> {
    let cast = move |text: String| Expr::literal(format!("{}::{}", quote(&text), ty.sql()));
    let value: BoxedStrategy<Expr> = match ty {
        Ty::Int4 => prop_oneof![
            -50_i32..=50,
            select(vec![
                i32::MIN,
                i32::MIN + 1,
                -1,
                0,
                1,
                i32::MAX - 1,
                i32::MAX
            ]),
        ]
        .prop_map(move |v| cast(v.to_string()))
        .boxed(),
        Ty::Int8 => prop_oneof![
            -50_i64..=50,
            select(vec![
                i64::MIN,
                i64::MIN + 1,
                i64::from(i32::MIN),
                i64::from(i32::MAX),
                4_294_967_296,
                -1,
                0,
                1,
                i64::MAX
            ]),
        ]
        .prop_map(move |v| cast(v.to_string()))
        .boxed(),
        Ty::Int2 => prop_oneof![
            -50_i16..=50,
            select(vec![i16::MIN, i16::MIN + 1, -1, 0, 1, i16::MAX]),
        ]
        .prop_map(move |v| cast(v.to_string()))
        .boxed(),
        Ty::Numeric => prop_oneof![
            (-10_000_i64..=10_000, 0_u32..=4).prop_map(|(value, scale)| {
                let divisor = 10_i64.pow(scale);
                let sign = if value < 0 { "-" } else { "" };
                let magnitude = value.unsigned_abs();
                if scale == 0 {
                    format!("{value}")
                } else {
                    format!(
                        "{sign}{}.{:0width$}",
                        magnitude / divisor.unsigned_abs(),
                        magnitude % divisor.unsigned_abs(),
                        width = scale as usize
                    )
                }
            }),
            select(vec![
                "0",
                "-0.01",
                "9999999999.99",
                "999999999999999999",
                "0.000001",
                "1e20",
                "NaN",
            ])
            .prop_map(String::from),
        ]
        .prop_map(cast)
        .boxed(),
        Ty::Float8 => prop_oneof![
            (-200_i32..=200).prop_map(|quarters| (f64::from(quarters) / 4.0).to_string()),
            select(vec!["NaN", "Infinity", "-Infinity", "-0", "1e308"]).prop_map(String::from),
        ]
        .prop_map(cast)
        .boxed(),
        Ty::Text => prop_oneof![
            vec(select(vec!['a', 'b', ' ', '%', '_', 'A', 'z']), 0..4)
                .prop_map(|chars| chars.into_iter().collect::<String>()),
            select(vec!["", " ", "zzzz", "a%b"]).prop_map(String::from),
        ]
        .prop_map(cast)
        .boxed(),
        Ty::Date => (-2000_i32..=2000)
            .prop_map(|days| Expr::literal(format!("(date '2000-01-01' + {days})")))
            .boxed(),
        Ty::Timestamp => (-48_000_i32..=48_000)
            .prop_map(|hours| {
                Expr::literal(format!(
                    "(timestamp '2000-01-01' + interval '{hours} hours')"
                ))
            })
            .boxed(),
        Ty::Bool => select(vec!["true", "false"])
            .prop_map(Expr::literal)
            .boxed(),
    };
    Union::new_weighted(vec![
        (12, value),
        (
            1,
            Just(Expr::literal(format!("NULL::{}", ty.sql()))).boxed(),
        ),
    ])
    .boxed()
}

/// A leaf of the type: a column in scope three times in four, else a
/// literal.
fn leaf(ty: Ty, scope: &Scope) -> BoxedStrategy<Expr> {
    let columns = scope.of_type(ty);
    if columns.is_empty() {
        return literal(ty);
    }
    Union::new_weighted(vec![(3, select(columns).boxed()), (1, literal(ty))]).boxed()
}

/// Expressions of every type up to `depth`.
pub fn expressions(scope: &Scope, depth: u32) -> HashMap<Ty, BoxedStrategy<Expr>> {
    let mut level: HashMap<Ty, BoxedStrategy<Expr>> =
        TYPES.iter().map(|&ty| (ty, leaf(ty, scope))).collect();
    for _ in 0..depth {
        let next = TYPES
            .iter()
            .map(|&ty| {
                let mut options = vec![(4, level[&ty].clone())];
                options.extend(composites(ty, &level, scope));
                (ty, Union::new_weighted(options).boxed())
            })
            .collect();
        level = next;
    }
    level
}

/// The operations that give a value of `ty` from the expressions of the
/// previous depth, `below`.
fn composites(
    ty: Ty,
    below: &HashMap<Ty, BoxedStrategy<Expr>>,
    scope: &Scope,
) -> Vec<(u32, BoxedStrategy<Expr>)> {
    let same = || below[&ty].clone();
    let bools = || below[&Ty::Bool].clone();
    let binary = |ops: Vec<&'static str>| {
        (select(ops), same(), same())
            .prop_map(|(op, left, right)| Expr::Binary {
                op,
                left: boxed(left),
                right: boxed(right),
            })
            .boxed()
    };
    let case = (bools(), same(), same())
        .prop_map(|(when, then, otherwise)| Expr::Case {
            when: boxed(when),
            then: boxed(then),
            otherwise: boxed(otherwise),
        })
        .boxed();
    let coalesce = (same(), same())
        .prop_map(|(first, second)| Expr::Call {
            name: "coalesce",
            args: vec![first, second],
        })
        .boxed();
    let call = |name: &'static str| {
        same()
            .prop_map(move |arg| Expr::Call {
                name,
                args: vec![arg],
            })
            .boxed()
    };
    let cast_from = |from: Ty| {
        below[&from]
            .clone()
            .prop_map(move |arg| Expr::Cast {
                arg: boxed(arg),
                ty,
            })
            .boxed()
    };
    let mut options = vec![(1, case), (1, coalesce)];
    match ty {
        Ty::Int2 | Ty::Int4 | Ty::Int8 => {
            options.push((4, binary(vec!["+", "-", "*", "/", "%"])));
            options.push((
                1,
                same()
                    .prop_map(|arg| Expr::Prefix {
                        op: "-",
                        arg: boxed(arg),
                    })
                    .boxed(),
            ));
            options.push((1, call("abs")));
            let other = if ty == Ty::Int8 { Ty::Int4 } else { Ty::Int8 };
            options.push((1, cast_from(other)));
            if ty == Ty::Int4 {
                options.push((
                    1,
                    below[&Ty::Text]
                        .clone()
                        .prop_map(|arg| Expr::Call {
                            name: "length",
                            args: vec![arg],
                        })
                        .boxed(),
                ));
            }
        }
        Ty::Numeric => {
            options.push((4, binary(vec!["+", "-", "*", "/"])));
            options.push((1, call("abs")));
            options.push((
                1,
                (same(), 0_i32..=4)
                    .prop_map(|(arg, places)| Expr::Call {
                        name: "round",
                        args: vec![arg, Expr::literal(places.to_string())],
                    })
                    .boxed(),
            ));
            options.push((1, cast_from(Ty::Int4)));
            options.push((1, cast_from(Ty::Int8)));
        }
        Ty::Float8 => {
            options.push((3, binary(vec!["+", "-", "*"])));
            options.push((1, call("abs")));
            options.push((1, cast_from(Ty::Int4)));
        }
        Ty::Text => {
            options.push((2, binary(vec!["||"])));
            options.push((1, call("lower")));
            options.push((1, call("upper")));
            options.push((
                1,
                (same(), 0_i32..=4, 0_i32..=4)
                    .prop_map(|(arg, from, count)| Expr::Call {
                        name: "substr",
                        args: vec![
                            arg,
                            Expr::literal(from.to_string()),
                            Expr::literal(count.to_string()),
                        ],
                    })
                    .boxed(),
            ));
            options.push((1, cast_from(Ty::Int4)));
        }
        Ty::Date => {
            options.push((
                2,
                (same(), select(vec!["+", "-"]), -40_i32..=40)
                    .prop_map(|(date, op, days)| Expr::Binary {
                        op,
                        left: boxed(date),
                        right: boxed(Expr::literal(format!("{days}"))),
                    })
                    .boxed(),
            ));
        }
        Ty::Timestamp => {
            options.push((1, cast_from(Ty::Date)));
            options.push((
                1,
                (same(), -100_i32..=100)
                    .prop_map(|(stamp, hours)| Expr::Binary {
                        op: "+",
                        left: boxed(stamp),
                        right: boxed(Expr::literal(format!("interval '{hours} hours'"))),
                    })
                    .boxed(),
            ));
        }
        Ty::Bool => options.extend(predicates(below, scope)),
    }
    options
}

/// The conditions over expressions of the previous depth: comparisons,
/// NULL tests, ranges, lists, patterns and their connectives.
fn predicates(
    below: &HashMap<Ty, BoxedStrategy<Expr>>,
    scope: &Scope,
) -> Vec<(u32, BoxedStrategy<Expr>)> {
    // Compare the types the query reads more than the others.
    let mut read: Vec<Ty> = scope
        .columns
        .iter()
        .map(|(_, column)| column.ty)
        .filter(|ty| *ty != Ty::Bool)
        .collect();
    read.dedup();
    if read.is_empty() {
        read.push(Ty::Int4);
    }
    let any_compared = select(read).prop_flat_map({
        let below = below.clone();
        move |ty| {
            // The right side: the same type, or another of the family.
            let partner = if ty.is_integer() {
                prop_oneof![
                    Just(ty),
                    select(vec![Ty::Int2, Ty::Int4, Ty::Int8, Ty::Numeric])
                ]
                .boxed()
            } else {
                Just(ty).boxed()
            };
            let below = below.clone();
            (Just(ty), partner)
                .prop_flat_map(move |(left, right)| (below[&left].clone(), below[&right].clone()))
        }
    });
    let comparison = (select(vec!["=", "<>", "<", "<=", ">", ">="]), any_compared)
        .prop_map(|(op, (left, right))| Expr::Binary {
            op,
            left: boxed(left),
            right: boxed(right),
        })
        .boxed();
    let is_null = (
        select(TYPES.to_vec()).prop_flat_map({
            let below = below.clone();
            move |ty| below[&ty].clone()
        }),
        any::<bool>(),
    )
        .prop_map(|(arg, negated)| Expr::IsNull {
            arg: boxed(arg),
            negated,
        })
        .boxed();
    let between = (
        select(vec![Ty::Int4, Ty::Numeric, Ty::Date]).prop_flat_map({
            let below = below.clone();
            move |ty| (below[&ty].clone(), below[&ty].clone(), below[&ty].clone())
        }),
        any::<bool>(),
    )
        .prop_map(|((arg, low, high), negated)| Expr::Between {
            arg: boxed(arg),
            low: boxed(low),
            high: boxed(high),
            negated,
        })
        .boxed();
    let in_list = (
        select(vec![Ty::Int4, Ty::Int8, Ty::Text, Ty::Date, Ty::Numeric]).prop_flat_map({
            let below = below.clone();
            move |ty| (below[&ty].clone(), vec(literal(ty), 1..8))
        }),
        any::<bool>(),
    )
        .prop_map(|((arg, list), negated)| Expr::InList {
            arg: boxed(arg),
            list,
            negated,
        })
        .boxed();
    let like = (
        below[&Ty::Text].clone(),
        vec(select(vec!['a', 'b', ' ', '%', '_', 'A']), 0..5)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
        any::<bool>(),
    )
        .prop_map(|(arg, pattern, negated)| Expr::Like {
            arg: boxed(arg),
            pattern,
            negated,
        })
        .boxed();
    let bools = || below[&Ty::Bool].clone();
    let connective = (select(vec!["AND", "OR"]), bools(), bools())
        .prop_map(|(op, left, right)| Expr::Binary {
            op,
            left: boxed(left),
            right: boxed(right),
        })
        .boxed();
    let not = bools()
        .prop_map(|arg| Expr::Prefix {
            op: "NOT",
            arg: boxed(arg),
        })
        .boxed();
    vec![
        (6, comparison),
        (2, is_null),
        (1, between),
        (2, in_list),
        (2, like),
        (3, connective),
        (1, not),
    ]
}

/// An expression of any type, with its type.
fn any_expr(exprs: &HashMap<Ty, BoxedStrategy<Expr>>) -> BoxedStrategy<(Ty, Expr)> {
    Union::new(
        TYPES
            .iter()
            .map(|&ty| exprs[&ty].clone().prop_map(move |expr| (ty, expr))),
    )
    .boxed()
}

/// An output that stands for equal values: a group's key, a distinct row,
/// a set operation's row, the least or greatest value. Equal numerics may
/// differ in scale (0 and 0.0) and equal float8 in sign (0 and -0); which
/// of them the output shows is not defined, in the core either (a hash
/// grouping keeps the first one met, a sorted one the first after a sort),
/// so such outputs are normalized: trim_scale, and adding zero.
fn representative(ty: Ty, expr: Expr) -> Expr {
    match ty {
        Ty::Numeric => Expr::Call {
            name: "trim_scale",
            args: vec![expr],
        },
        Ty::Float8 => Expr::Binary {
            op: "+",
            left: boxed(expr),
            right: boxed(Expr::literal("0::float8")),
        },
        _ => expr,
    }
}

/// The settings: serial three times in four, work_mem small enough for the
/// hash tables and sorts to spill now and then.
fn settings() -> impl Strategy<Value = Settings> {
    (
        prop_oneof![3 => Just(0_u8), 1 => Just(2_u8)],
        select(vec!["4MB", "256kB", "64kB"]),
    )
        .prop_map(|(workers, work_mem)| Settings { workers, work_mem })
}

/// How a table joins the ones before it: inner joins most often.
fn join_kind() -> impl Strategy<Value = JoinKind> {
    prop_oneof![
        4 => Just(JoinKind::Inner),
        2 => Just(JoinKind::Left),
        1 => Just(JoinKind::Right),
        1 => Just(JoinKind::Full),
    ]
}

/// The FROM list: a table and up to two more joined to it, one table
/// half the time.
fn from_list() -> impl Strategy<Value = (Table, Vec<(JoinKind, Table)>)> {
    (
        select(TABLES.to_vec()),
        prop_oneof![
            4 => Just(0_usize),
            3 => Just(1_usize),
            1 => Just(2_usize),
        ]
        .prop_flat_map(|joins| vec((join_kind(), select(TABLES.to_vec())), joins)),
    )
}

/// Whether two columns can be a join key or a semi join's pair: integers
/// with integers, text with text.
fn keyed(left: Ty, right: Ty) -> bool {
    (left.is_integer() && right.is_integer()) || (left == Ty::Text && right == Ty::Text)
}

/// The pairs of a column of `table` and a column of `scope` that can be
/// joined.
fn key_pairs(table: Table, scope: &Scope) -> Vec<(&'static str, Expr)> {
    table
        .columns
        .iter()
        .flat_map(|column| {
            scope
                .columns
                .iter()
                .filter(move |(_, other)| keyed(column.ty, other.ty))
                .map(move |(index, other)| {
                    (
                        column.name,
                        Expr::Column {
                            table: *index,
                            name: other.name,
                        },
                    )
                })
        })
        .collect()
}

/// The joins of a FROM list: one or two key pairs each, and an extra
/// condition over the tables so far a time in four.
fn joins(first: Table, joined: Vec<(JoinKind, Table)>) -> BoxedStrategy<Vec<Join>> {
    let mut tables = vec![first];
    let mut strategies = Vec::new();
    for (kind, table) in joined {
        let before = Scope::of(&tables);
        tables.push(table);
        let with = expressions(&Scope::of(&tables), 1);
        strategies.push(
            (
                vec(select(key_pairs(table, &before)), 1..=2),
                proptest::option::weighted(0.25, with[&Ty::Bool].clone()),
            )
                .prop_map(move |(mut keys, extra)| {
                    keys.dedup_by(|a, b| a.0 == b.0);
                    Join {
                        kind,
                        table,
                        keys,
                        extra,
                    }
                })
                .boxed(),
        );
    }
    strategies.boxed()
}

/// A semi or anti join in the condition: `[NOT] EXISTS` over a table on a
/// key pair, with a condition over the inner table now and then.
fn exists(tables: &[Table]) -> BoxedStrategy<Expr> {
    let outer = Scope::of(tables);
    let depth = tables.len();
    select(TABLES.to_vec())
        .prop_flat_map(move |inner| {
            let pairs = key_pairs(inner, &outer);
            let mut all = outer.clone();
            all.columns
                .extend(inner.columns.iter().map(|column| (depth, *column)));
            let with = expressions(&all, 1);
            (
                select(pairs),
                proptest::option::weighted(0.5, with[&Ty::Bool].clone()),
                prop_oneof![2 => Just(false), 1 => Just(true)],
            )
                .prop_map(move |((column, outer), filter, negated)| Expr::Exists {
                    table: inner,
                    inner: column,
                    outer: boxed(outer),
                    filter: filter.map(boxed),
                    negated,
                })
        })
        .boxed()
}

/// The condition of a query: an expression, a semi or anti join, or both.
fn condition(
    tables: &[Table],
    exprs: &HashMap<Ty, BoxedStrategy<Expr>>,
) -> BoxedStrategy<Option<Expr>> {
    let plain = exprs[&Ty::Bool].clone();
    let semi = exists(tables);
    prop_oneof![
        2 => Just(None),
        6 => plain.clone().prop_map(Some),
        1 => semi.clone().prop_map(Some),
        1 => (plain, semi).prop_map(|(plain, semi)| Some(Expr::Binary {
            op: "AND",
            left: boxed(plain),
            right: boxed(semi),
        })),
    ]
    .boxed()
}

/// An aggregate over the expressions of depth 1: the ones TessAgg takes
/// natively and through the core's transition functions, and none whose
/// result depends on the order rows come in (no sum or avg of float8).
fn aggregate(exprs: &HashMap<Ty, BoxedStrategy<Expr>>) -> BoxedStrategy<Expr> {
    let call = |name: &'static str, types: Vec<Ty>, distinct: bool| {
        let exprs = exprs.clone();
        select(types)
            .prop_flat_map(move |ty| (Just(ty), exprs[&ty].clone()))
            .prop_map(move |(ty, arg)| {
                let aggregate = Expr::Aggregate {
                    name,
                    arg: Some(boxed(arg)),
                    distinct,
                };
                // The least or greatest of equal values is any of them.
                if matches!(name, "min" | "max") {
                    representative(ty, aggregate)
                } else {
                    aggregate
                }
            })
            .boxed()
    };
    let exact = vec![Ty::Int2, Ty::Int4, Ty::Int8, Ty::Numeric];
    let ordered = vec![
        Ty::Int4,
        Ty::Int8,
        Ty::Numeric,
        Ty::Float8,
        Ty::Text,
        Ty::Date,
        Ty::Timestamp,
    ];
    Union::new_weighted(vec![
        (
            2,
            Just(Expr::Aggregate {
                name: "count",
                arg: None,
                distinct: false,
            })
            .boxed(),
        ),
        (2, call("count", TYPES.to_vec(), false)),
        (1, call("count", vec![Ty::Int4, Ty::Int8, Ty::Text], true)),
        (3, call("sum", exact.clone(), false)),
        (1, call("sum", vec![Ty::Int4, Ty::Int8], true)),
        (2, call("avg", exact, false)),
        (2, call("min", ordered.clone(), false)),
        (2, call("max", ordered, false)),
        (1, call("bool_and", vec![Ty::Bool], false)),
        (1, call("bool_or", vec![Ty::Bool], false)),
    ])
    .boxed()
}

/// The grouping of a query and its outputs: up to two keys (none is a
/// plain aggregate), the keys then one to three aggregates as the outputs,
/// and a condition on the groups' row counts now and then.
fn grouped(exprs: &HashMap<Ty, BoxedStrategy<Expr>>) -> BoxedStrategy<(Vec<Expr>, Grouping)> {
    let key_types = vec![
        Ty::Int4,
        Ty::Int8,
        Ty::Int2,
        Ty::Text,
        Ty::Date,
        Ty::Bool,
        Ty::Numeric,
    ];
    let key = {
        let exprs = exprs.clone();
        select(key_types).prop_flat_map(move |ty| (Just(ty), exprs[&ty].clone()))
    };
    let having = (
        select(vec!["=", "<>", "<", ">"]),
        aggregate(exprs).prop_filter("a count", |agg| {
            matches!(agg, Expr::Aggregate { name: "count", .. })
        }),
        0_i64..20,
    )
        .prop_map(|(op, count, limit)| Expr::Binary {
            op,
            left: boxed(count),
            right: boxed(Expr::literal(format!("{limit}"))),
        });
    (
        vec(key, 0..=2),
        vec(aggregate(exprs), 1..=3),
        proptest::option::weighted(0.3, having),
    )
        .prop_map(|(keys, aggregates, having)| {
            let items = keys
                .iter()
                .map(|(ty, key)| representative(*ty, key.clone()))
                .chain(aggregates)
                .collect();
            let keys = keys.into_iter().map(|(_, key)| key).collect();
            (items, Grouping { keys, having })
        })
        .boxed()
}

/// A SELECT over a FROM list: expressions or a grouping as its outputs, a
/// condition, DISTINCT now and then, a limit now and then.
fn select_query() -> impl Strategy<Value = Select> {
    from_list().prop_flat_map(|(first, joined)| {
        let tables: Vec<Table> = std::iter::once(first)
            .chain(joined.iter().map(|(_, table)| *table))
            .collect();
        let exprs = expressions(&Scope::of(&tables), DEPTH);
        let shallow = expressions(&Scope::of(&tables), 1);
        let plain = (
            vec(any_expr(&exprs), 1..=4),
            proptest::option::weighted(0.15, Just(())),
        )
            .prop_map(|(items, distinct)| {
                let distinct = distinct.is_some();
                let items = items
                    .into_iter()
                    .map(|(ty, item)| {
                        if distinct {
                            representative(ty, item)
                        } else {
                            item
                        }
                    })
                    .collect();
                (items, None, distinct)
            });
        let aggregated =
            grouped(&shallow).prop_map(|(items, grouping)| (items, Some(grouping), false));
        (
            joins(first, joined),
            prop_oneof![3 => plain, 2 => aggregated],
            condition(&tables, &exprs),
            proptest::option::weighted(0.2, 1_u32..20),
        )
            .prop_map(
                move |(joins, (items, grouping, distinct), filter, limit)| Select {
                    distinct,
                    items,
                    from: first,
                    joins,
                    filter,
                    grouping,
                    limit,
                },
            )
    })
}

/// A SELECT whose outputs have the given types, for a set operation.
fn typed_select(types: Vec<Ty>) -> impl Strategy<Value = Select> {
    from_list().prop_flat_map(move |(first, joined)| {
        let tables: Vec<Table> = std::iter::once(first)
            .chain(joined.iter().map(|(_, table)| *table))
            .collect();
        let exprs = expressions(&Scope::of(&tables), DEPTH);
        // A set operation matches rows by equality: its outputs stand for
        // equal values.
        let items: Vec<BoxedStrategy<Expr>> = types
            .iter()
            .map(|&ty| {
                exprs[&ty]
                    .clone()
                    .prop_map(move |expr| representative(ty, expr))
                    .boxed()
            })
            .collect();
        (
            joins(first, joined),
            items,
            condition(&tables, &exprs),
            proptest::option::weighted(0.15, Just(())),
        )
            .prop_map(move |(joins, items, filter, distinct)| Select {
                distinct: distinct.is_some(),
                items,
                from: first,
                joins,
                filter,
                grouping: None,
                limit: None,
            })
    })
}

/// Two SELECTs of one row type and how they combine.
fn set_operation() -> impl Strategy<Value = (Select, SetOp, Select)> {
    let types = vec(
        select(vec![
            Ty::Int4,
            Ty::Int8,
            Ty::Text,
            Ty::Numeric,
            Ty::Date,
            Ty::Bool,
        ]),
        1..=3,
    );
    let op = select(vec![
        SetOp::Union,
        SetOp::UnionAll,
        SetOp::Intersect,
        SetOp::IntersectAll,
        SetOp::Except,
        SetOp::ExceptAll,
    ]);
    (types, op)
        .prop_flat_map(|(types, op)| (typed_select(types.clone()), Just(op), typed_select(types)))
}

/// A query and its settings: a SELECT, or a set operation of two, one
/// time in eight.
pub fn query() -> impl Strategy<Value = Query> {
    let single = select_query().prop_map(|select| (select, None));
    let combined = set_operation().prop_map(|(left, op, right)| (left, Some((op, right))));
    (prop_oneof![7 => single, 1 => combined], settings()).prop_map(|((select, set), settings)| {
        Query {
            select,
            set,
            settings,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

    fn runner(seed: u8) -> TestRunner {
        TestRunner::new_with_rng(
            Config::default(),
            TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]),
        )
    }

    #[test]
    fn one_seed_gives_one_query() {
        let first = query().new_tree(&mut runner(7)).unwrap().current();
        let again = query().new_tree(&mut runner(7)).unwrap().current();
        assert_eq!(first, again);
        assert!(first.sql().starts_with("SELECT "));
    }

    #[test]
    fn integer_literals_keep_their_sign_inside_the_cast() {
        let mut runner = runner(1);
        for _ in 0..200 {
            let Expr::Literal(sql) = literal(Ty::Int4).new_tree(&mut runner).unwrap().current()
            else {
                panic!("a literal");
            };
            assert!(sql.starts_with('\'') || sql.starts_with("NULL"), "{sql}");
        }
    }
}
