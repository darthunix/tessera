//! The 22 queries: their text, in bench/tpch/qNN.sql with the validation
//! parameters of the specification, and what the check needs to know of
//! each answer.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::compare::Kind::{self, Exact, Ratio, Sum};
use crate::compare::Shape;
use crate::config::Scale;

/// A query of TPC-H.
#[derive(Debug)]
pub struct Query {
    pub number: u8,
    pub title: &'static str,
    /// What each output column holds, for the precision of clause 2.1.3.5.
    pub kinds: &'static [Kind],
    /// The output columns of ORDER BY, by position.
    pub keys: &'static [usize],
    pub limit: Option<usize>,
}

impl Query {
    /// `Q01`, as tables and files name the queries.
    pub fn name(&self) -> String {
        format!("Q{:02}", self.number)
    }

    pub fn shape(&self) -> Shape<'static> {
        Shape {
            kinds: self.kinds,
            keys: self.keys,
            limit: self.limit,
        }
    }

    /// The text to prepare: the file without its last semicolon, Q11's
    /// FRACTION replaced by 0.0001 / SF, as the specification sets it.
    pub fn text(&self, root: &Path, scale: &Scale) -> Result<String> {
        let path = root.join(format!("bench/tpch/q{:02}.sql", self.number));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let text = text.trim_end();
        let text = text.strip_suffix(';').unwrap_or(text);
        Ok(text.replace(":FRACTION", &fraction(scale)))
    }
}

/// The 22 queries in their order.
pub const QUERIES: [Query; 22] = [
    query(
        1,
        "pricing summary report",
        &[Exact, Exact, Sum, Sum, Sum, Sum, Ratio, Ratio, Ratio, Exact],
        &[0, 1],
        None,
    ),
    query(
        2,
        "minimum cost supplier",
        &[Exact; 8],
        &[0, 2, 1, 3],
        Some(100),
    ),
    query(
        3,
        "shipping priority",
        &[Exact, Sum, Exact, Exact],
        &[1, 2],
        Some(10),
    ),
    query(4, "order priority checking", &[Exact, Exact], &[0], None),
    query(5, "local supplier volume", &[Exact, Sum], &[1], None),
    query(6, "forecasting revenue change", &[Sum], &[], None),
    query(
        7,
        "volume shipping",
        &[Exact, Exact, Exact, Sum],
        &[0, 1, 2],
        None,
    ),
    query(8, "national market share", &[Exact, Ratio], &[0], None),
    query(
        9,
        "product type profit measure",
        &[Exact, Exact, Sum],
        &[0, 1],
        None,
    ),
    query(
        10,
        "returned item reporting",
        &[Exact, Exact, Sum, Exact, Exact, Exact, Exact, Exact],
        &[2],
        Some(20),
    ),
    query(
        11,
        "important stock identification",
        &[Exact, Sum],
        &[1],
        None,
    ),
    query(
        12,
        "shipping modes and order priority",
        &[Exact, Sum, Sum],
        &[0],
        None,
    ),
    query(13, "customer distribution", &[Exact, Exact], &[1, 0], None),
    query(14, "promotion effect", &[Ratio], &[], None),
    query(
        15,
        "top supplier",
        &[Exact, Exact, Exact, Exact, Sum],
        &[0],
        None,
    ),
    query(
        16,
        "parts/supplier relationship",
        &[Exact; 4],
        &[3, 0, 1, 2],
        None,
    ),
    query(17, "small-quantity-order revenue", &[Ratio], &[], None),
    query(
        18,
        "large volume customer",
        &[Exact, Exact, Exact, Exact, Exact, Sum],
        &[4, 3],
        Some(100),
    ),
    query(19, "discounted revenue", &[Sum], &[], None),
    query(20, "potential part promotion", &[Exact, Exact], &[0], None),
    query(
        21,
        "suppliers who kept orders waiting",
        &[Exact, Exact],
        &[1, 0],
        Some(100),
    ),
    query(
        22,
        "global sales opportunity",
        &[Exact, Exact, Sum],
        &[0],
        None,
    ),
];

const fn query(
    number: u8,
    title: &'static str,
    kinds: &'static [Kind],
    keys: &'static [usize],
    limit: Option<usize>,
) -> Query {
    Query {
        number,
        title,
        kinds,
        keys,
        limit,
    }
}

/// The short set for A/B comparisons of a change, after Kersten et al.,
/// "Everything You Always Wanted to Know About Compiled and Vectorized
/// Queries But Were Afraid to Ask" (2018): aggregation over a scan (Q1,
/// Q6), joins with a selective filter (Q3, Q9) and a large grouping (Q18).
pub const CORE: [u8; 5] = [1, 3, 6, 9, 18];

pub fn get(number: u8) -> &'static Query {
    &QUERIES[usize::from(number) - 1]
}

/// FRACTION of Q11, 0.0001 / SF, as a decimal literal.
pub fn fraction(scale: &Scale) -> String {
    let text = format!("{:.12}", 0.0001 / scale.factor());
    let text = text.trim_end_matches('0');
    text.strip_suffix('.').unwrap_or(text).to_string()
}

/// A selection of queries: `all`, `core`, or numbers and ranges such as
/// `1,3,6` and `1-5,9`, in the order of their numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection(pub Vec<u8>);

impl Selection {
    pub fn parse(text: &str) -> Result<Selection> {
        let mut numbers = Vec::new();
        for item in text.split(',').map(str::trim) {
            match item.to_ascii_lowercase().as_str() {
                "all" => numbers.extend(1..=22),
                "core" => numbers.extend(CORE),
                _ => {
                    let number = |text: &str| -> Result<u8> {
                        let text = text.trim().trim_start_matches(['q', 'Q']);
                        match text.parse::<u8>() {
                            Ok(number @ 1..=22) => Ok(number),
                            _ => bail!("queries are numbered 1 to 22, not {text:?}"),
                        }
                    };
                    match item.split_once('-') {
                        Some((first, last)) => {
                            let (first, last) = (number(first)?, number(last)?);
                            if first > last {
                                bail!("the range {item} is empty");
                            }
                            numbers.extend(first..=last);
                        }
                        None => numbers.push(number(item)?),
                    }
                }
            }
        }
        numbers.sort_unstable();
        numbers.dedup();
        Ok(Selection(numbers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_in_order_with_keys_among_their_columns() {
        for (index, query) in QUERIES.iter().enumerate() {
            assert_eq!(usize::from(query.number), index + 1);
            assert!(
                query.keys.iter().all(|&key| key < query.kinds.len()),
                "{}",
                query.name()
            );
        }
        assert_eq!(get(18).name(), "Q18");
    }

    #[test]
    fn every_file_is_a_query_without_placeholders_but_fraction() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let scale = Scale::parse("1").unwrap();
        for query in &QUERIES {
            let text = query.text(&root, &scale).unwrap();
            assert!(!text.ends_with(';'), "{}", query.name());
            // A placeholder left, as :1 in the templates or :FRACTION.
            let placeholder = text
                .as_bytes()
                .windows(2)
                .any(|pair| pair[0] == b':' && pair[1].is_ascii_alphanumeric());
            assert!(!placeholder, "{}: {text}", query.name());
            let select = text.to_ascii_lowercase();
            assert!(select.contains("select"), "{}", query.name());
            assert_eq!(
                select.contains("\nlimit "),
                query.limit.is_some(),
                "{}",
                query.name()
            );
        }
        let q11 = get(11).text(&root, &Scale::parse("10").unwrap()).unwrap();
        assert!(q11.contains("* 0.00001\n"), "{q11}");
    }

    #[test]
    fn fraction_is_per_scale() {
        let fraction = |text| fraction(&Scale::parse(text).unwrap());
        assert_eq!(fraction("1"), "0.0001");
        assert_eq!(fraction("10"), "0.00001");
        assert_eq!(fraction("100"), "0.000001");
        assert_eq!(fraction("0.01"), "0.01");
    }

    #[test]
    fn selections() {
        let parse = |text| Selection::parse(text).unwrap().0;
        assert_eq!(parse("all"), (1..=22).collect::<Vec<u8>>());
        assert_eq!(parse("core"), vec![1, 3, 6, 9, 18]);
        assert_eq!(parse("6,1,3-5,q22,Q1"), vec![1, 3, 4, 5, 6, 22]);
        assert_eq!(parse("core,2"), vec![1, 2, 3, 6, 9, 18]);
        for wrong in ["0", "23", "5-3", "x", ""] {
            assert!(Selection::parse(wrong).is_err(), "{wrong:?}");
        }
    }
}
