//! ClickBench's 43 queries, `bench/clickbench/queries.sql`, one a line,
//! numbered from 1 as ClickBench's results number them, with what the
//! check needs of each: the output columns its ORDER BY sorts on, its
//! LIMIT and OFFSET, and the query without them.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

/// How many queries ClickBench has.
pub const COUNT: usize = 43;

/// The output columns, by position from 0, that each query's ORDER BY
/// sorts on, where they are output columns. A query that does not sort
/// has none, and its rows are compared as one run; Q25, which sorts on a
/// column it does not return, is checked through [`WIDENED`] too.
const KEYS: [&[usize]; COUNT] = [
    &[],  // Q1 count(*)
    &[],  // Q2
    &[],  // Q3
    &[],  // Q4
    &[],  // Q5
    &[],  // Q6
    &[],  // Q7
    &[1], // Q8 ORDER BY count(*)
    &[1], // Q9 ORDER BY u
    &[2], // Q10 ORDER BY c
    &[1], // Q11 ORDER BY u
    &[2], // Q12 ORDER BY u
    &[1], // Q13 ORDER BY c
    &[1], // Q14 ORDER BY u
    &[2], // Q15 ORDER BY c
    &[1], // Q16 ORDER BY count(*)
    &[2], // Q17 ORDER BY count(*)
    &[],  // Q18 no ORDER BY
    &[3], // Q19 ORDER BY count(*)
    &[],  // Q20 no ORDER BY
    &[],  // Q21 count(*)
    &[2], // Q22 ORDER BY c
    &[3], // Q23 ORDER BY c
    &[4], // Q24 SELECT *, ORDER BY EventTime, the fifth column
    &[],  // Q25 ORDER BY EventTime, not returned
    &[0], // Q26 ORDER BY SearchPhrase
    &[0], // Q27 ORDER BY EventTime, SearchPhrase: rows of equal keys are equal
    &[1], // Q28 ORDER BY l
    &[1], // Q29 ORDER BY l
    &[],  // Q30 sums
    &[2], // Q31 ORDER BY c
    &[2], // Q32 ORDER BY c
    &[2], // Q33 ORDER BY c
    &[1], // Q34 ORDER BY c
    &[2], // Q35 ORDER BY c
    &[4], // Q36 ORDER BY c
    &[1], // Q37 ORDER BY PageViews
    &[1], // Q38 ORDER BY PageViews
    &[1], // Q39 ORDER BY PageViews
    &[5], // Q40 ORDER BY PageViews
    &[2], // Q41 ORDER BY PageViews
    &[2], // Q42 ORDER BY PageViews
    &[0], // Q43 ORDER BY DATE_TRUNC('minute', EventTime), the column M
];

/// The queries whose sort key is not among the columns they return, with
/// the query that returns it too, by position, which the check compares
/// in every mode beside the query itself: Q25's ten rows are any ten
/// phrases unless their `EventTime` is the least.
pub const WIDENED: [(usize, &str, &[usize]); 1] = [(
    25,
    "SELECT SearchPhrase, EventTime FROM hits WHERE SearchPhrase <> '' \
     ORDER BY EventTime LIMIT 10",
    &[1],
)];

/// One of ClickBench's queries.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// From 1.
    pub number: usize,
    /// As ClickBench gives it, without the final semicolon.
    pub text: String,
    pub keys: &'static [usize],
    pub limit: Option<usize>,
    pub offset: usize,
    /// The text without its LIMIT and OFFSET, where it has a LIMIT: the
    /// result that the rows past a cut must be among.
    pub full: Option<String>,
}

impl Query {
    pub fn name(&self) -> String {
        format!("Q{}", self.number)
    }

    /// The query without its LIMIT, OFFSET and ORDER BY, where it has a
    /// LIMIT: the rows the companion query looks among, unsorted.
    pub fn unordered(&self) -> Option<&str> {
        let full = self.full.as_deref()?;
        Some(full.rfind(" ORDER BY ").map_or(full, |at| &full[..at]))
    }

    /// The query that returns its hidden sort key too, as a query of its
    /// own, where [`WIDENED`] has one.
    pub fn widened(&self) -> Option<Query> {
        let &(_, text, keys) = WIDENED.iter().find(|(query, ..)| *query == self.number)?;
        let mut query = Query::parse(self.number, text).ok()?;
        query.keys = keys;
        Some(query)
    }

    fn parse(number: usize, line: &str) -> Result<Query> {
        let text = line.trim().trim_end_matches(';').trim_end().to_string();
        let words: Vec<&str> = text.split_whitespace().collect();
        let tail = |n: usize| words.len().checked_sub(n).map(|at| &words[at..]);
        let number_at = |word: &str| -> Result<usize> {
            word.parse()
                .with_context(|| format!("Q{number}: {word:?} is not a count of rows"))
        };
        let (limit, offset, cut) = match (tail(4), tail(2)) {
            (Some(&[limit_word, limit, offset_word, offset]), _)
                if limit_word.eq_ignore_ascii_case("LIMIT")
                    && offset_word.eq_ignore_ascii_case("OFFSET") =>
            {
                (Some(number_at(limit)?), number_at(offset)?, 4)
            }
            (_, Some(&[limit_word, limit])) if limit_word.eq_ignore_ascii_case("LIMIT") => {
                (Some(number_at(limit)?), 0, 2)
            }
            _ => (None, 0, 0),
        };
        let full = limit.map(|_| words[..words.len() - cut].join(" "));
        Ok(Query {
            number,
            text,
            keys: KEYS[number - 1],
            limit,
            offset,
            full,
        })
    }
}

/// The queries of `bench/clickbench/queries.sql` under `root`.
pub fn load(root: &Path) -> Result<Vec<Query>> {
    let path = root.join("bench/clickbench/queries.sql");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    ensure!(
        lines.len() == COUNT,
        "{} holds {} queries, not {COUNT}",
        path.display(),
        lines.len()
    );
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| Query::parse(index + 1, line))
        .collect()
}

/// Which queries a command runs: numbers from 1, as `all`, `3`, `1-7` or
/// `1-7,24`, in ascending order without repeats.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection(pub Vec<usize>);

impl Selection {
    pub fn parse(text: &str) -> Result<Selection> {
        if text.trim() == "all" {
            return Ok(Selection((1..=COUNT).collect()));
        }
        let mut numbers = Vec::new();
        for part in text.split(',') {
            let part = part.trim();
            let (low, high) = part.split_once('-').unwrap_or((part, part));
            let parse = |word: &str| -> Result<usize> {
                let number: usize = word
                    .trim()
                    .parse()
                    .with_context(|| format!("{word:?} is not a query's number"))?;
                if !(1..=COUNT).contains(&number) {
                    bail!("ClickBench's queries are 1 to {COUNT}, not {number}");
                }
                Ok(number)
            };
            let (low, high) = (parse(low)?, parse(high)?);
            ensure!(low <= high, "{part:?} is an empty range");
            numbers.extend(low..=high);
        }
        numbers.sort_unstable();
        numbers.dedup();
        Ok(Selection(numbers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn the_queries_parse_with_their_limits() {
        let queries = load(&root()).unwrap();
        assert_eq!(queries.len(), COUNT);
        let q1 = &queries[0];
        assert_eq!(q1.text, "SELECT COUNT(*) FROM hits");
        assert_eq!((q1.limit, q1.offset, &q1.full), (None, 0, &None));
        let q18 = &queries[17];
        assert_eq!((q18.limit, q18.offset), (Some(10), 0));
        assert_eq!(
            q18.full.as_deref(),
            Some("SELECT UserID, SearchPhrase, COUNT(*) FROM hits GROUP BY UserID, SearchPhrase")
        );
        let q39 = &queries[38];
        assert_eq!(
            (q39.limit, q39.offset, q39.keys),
            (Some(10), 1000, &[1][..])
        );
        assert!(
            q39.full
                .as_deref()
                .unwrap()
                .ends_with("ORDER BY PageViews DESC")
        );
        assert!(q39.unordered().unwrap().ends_with("GROUP BY URL"));
        assert_eq!(q18.unordered(), q18.full.as_deref());
        assert_eq!(q1.unordered(), None);
        let q43 = &queries[42];
        assert!(
            q43.unordered()
                .unwrap()
                .ends_with("GROUP BY DATE_TRUNC('minute', EventTime)")
        );
        let limited = queries.iter().filter(|q| q.limit.is_some()).count();
        assert_eq!(limited, 32);
        assert_eq!(queries.iter().filter(|q| q.offset > 0).count(), 5);
    }

    #[test]
    fn a_key_names_an_output_column() {
        // Every key is within the columns the query returns: Q24 returns
        // the table's 105, the others no more than their commas give.
        for query in load(&root()).unwrap() {
            let select = &query.text[..query.text.find(" FROM hits").unwrap()];
            let columns = if select.ends_with('*') {
                105
            } else {
                select.matches(',').count() + 1
            };
            for &key in query.keys {
                assert!(key < columns, "{}: key {key} of {columns}", query.name());
            }
        }
    }

    #[test]
    fn a_cut_without_keys_is_of_a_query_that_does_not_sort() {
        // Rows past a LIMIT are compared by their keys alone: a query
        // that sorts but has no key among its columns would be checked by
        // its count alone, unless it is widened.
        for query in load(&root()).unwrap() {
            if query.limit.is_some() && query.keys.is_empty() {
                assert!(
                    !query.text.contains(" ORDER BY ") || query.widened().is_some(),
                    "{}",
                    query.name()
                );
            }
        }
        let q25 = &load(&root()).unwrap()[24];
        let widened = q25.widened().unwrap();
        assert_eq!((widened.keys, widened.limit), (&[1][..], Some(10)));
        assert!(
            widened
                .text
                .starts_with("SELECT SearchPhrase, EventTime FROM hits")
        );
    }

    #[test]
    fn selections_are_numbers_and_ranges() {
        assert_eq!(Selection::parse("all").unwrap().0.len(), COUNT);
        assert_eq!(Selection::parse("7, 1-3,2").unwrap().0, vec![1, 2, 3, 7]);
        assert_eq!(Selection::parse("43").unwrap().0, vec![43]);
        for wrong in ["0", "44", "3-1", "x", "1-", ""] {
            assert!(Selection::parse(wrong).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn a_limit_without_a_number_is_refused() {
        assert!(Query::parse(1, "SELECT 1 FROM hits LIMIT x;").is_err());
        let query = Query::parse(1, "SELECT a FROM hits ORDER BY a LIMIT 5 OFFSET 2;").unwrap();
        assert_eq!(query.full.as_deref(), Some("SELECT a FROM hits ORDER BY a"));
    }
}
