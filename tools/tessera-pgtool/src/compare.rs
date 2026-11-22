//! Whether two answers to a query are the same: the answer of TPC-H
//! against ours by the precision rules of clause 2.1.3.5, Tessera on
//! against off value for value, and another engine's answer against
//! PostgreSQL's with its numbers close.
//!
//! Rows are compared in their order, except that rows equal in the
//! columns of ORDER BY may come in any order, so such a run of rows is
//! compared as a multiset; and where the query's LIMIT cuts a run, the
//! rows that made it past the cut are any of the run's, so only their
//! sort columns are compared. An OFFSET cuts the first run in the same
//! way. Whether the rows of a cut run are rows of the query at all is for
//! the caller to check, against the query's result without its LIMIT.

/// A row of an answer, every value as text, NULL as `None`.
pub type Row = Vec<Option<String>>;

/// What an output column holds, which decides the precision clause
/// 2.1.3.5 asks of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A singleton value or a count: exactly.
    Exact,
    /// A sum: within 100.
    Sum,
    /// An average or a ratio: within 1 % once rounded to 0.01.
    Ratio,
}

/// How values are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precision {
    /// Byte for byte: Tessera on against off, which compute the same
    /// numeric values.
    Exact,
    /// By the kinds of the columns, clause 2.1.3.5: our answer against the
    /// published one, which rounds to two decimals and pads with spaces.
    Specification,
    /// In a column of the kinds `Sum` and `Ratio`, finite numbers within
    /// a relative 10^-9 of each other; in a column of the kind `Exact`,
    /// and any value that is not a finite number, byte for byte: another
    /// engine against PostgreSQL, where one computes an average in
    /// `double precision` and the other in `numeric`. The runs of rows
    /// equal in their keys are those of keys equal byte for byte, so
    /// that close keys do not chain into one run.
    Close,
}

/// What the comparison needs to know of a query's answer.
#[derive(Debug, Clone, Copy)]
pub struct Shape<'a> {
    pub kinds: &'a [Kind],
    /// The output columns of ORDER BY, by position.
    pub keys: &'a [usize],
    pub limit: Option<usize>,
    /// The rows the query's OFFSET skips: where it is above zero, the
    /// first run of rows equal in their keys may hold any of its run's
    /// rows, as the last one may past the LIMIT.
    pub offset: usize,
}

/// Compares `actual` with `expected`; the error names the first
/// difference.
pub fn compare(
    expected: &[Row],
    actual: &[Row],
    shape: &Shape<'_>,
    precision: Precision,
) -> Result<(), String> {
    if actual.len() != expected.len() {
        return Err(format!(
            "{} rows, expected {}",
            actual.len(),
            expected.len()
        ));
    }
    let width = shape.kinds.len();
    for (index, row) in expected.iter().chain(actual).enumerate() {
        if row.len() != width {
            let side = if index < expected.len() {
                "expected"
            } else {
                "actual"
            };
            return Err(format!("{side} row has {} columns, not {width}", row.len()));
        }
    }
    let cut = shape.limit == Some(expected.len());
    let runs = if precision == Precision::Close {
        Precision::Exact
    } else {
        precision
    };
    let mut start = 0;
    while start < expected.len() {
        let mut end = start + 1;
        while end < expected.len() && keys_equal(&expected[end - 1], &expected[end], shape, runs) {
            end += 1;
        }
        if cut && end == expected.len() || shape.offset > 0 && start == 0 {
            // Past a cut any rows of the run may come: their keys only.
            for index in start..end {
                if !keys_equal(&expected[start], &actual[index], shape, precision) {
                    return Err(difference(index, &expected[index], &actual[index]));
                }
            }
        } else {
            match_run(&expected[start..end], &actual[start..end], shape, precision).map_err(
                |offset| {
                    difference(
                        start + offset,
                        &expected[start + offset],
                        &actual[start + offset],
                    )
                },
            )?;
        }
        start = end;
    }
    Ok(())
}

/// Matches a run of rows equal in their keys as a multiset; the error is
/// the offset of the first expected row nothing matched.
fn match_run(
    expected: &[Row],
    actual: &[Row],
    shape: &Shape<'_>,
    precision: Precision,
) -> Result<(), usize> {
    if expected
        .iter()
        .zip(actual)
        .all(|(e, a)| rows_equal(e, a, shape, precision))
    {
        return Ok(());
    }
    let mut used = vec![false; actual.len()];
    for (offset, row) in expected.iter().enumerate() {
        let found =
            (0..actual.len()).find(|&k| !used[k] && rows_equal(row, &actual[k], shape, precision));
        match found {
            Some(k) => used[k] = true,
            None => return Err(offset),
        }
    }
    Ok(())
}

fn difference(index: usize, expected: &Row, actual: &Row) -> String {
    format!(
        "row {}: expected {}, got {}",
        index + 1,
        show(expected),
        show(actual)
    )
}

/// A row for a message: values trimmed, the whole cut at 200 characters.
fn show(row: &Row) -> String {
    let text = row
        .iter()
        .map(|value| value.as_deref().map_or("NULL", str::trim))
        .collect::<Vec<_>>()
        .join(" | ");
    let mut cut: String = text.chars().take(200).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    format!("[{cut}]")
}

fn rows_equal(expected: &Row, actual: &Row, shape: &Shape<'_>, precision: Precision) -> bool {
    (0..expected.len()).all(|c| values_equal(&expected[c], &actual[c], shape.kinds[c], precision))
}

fn keys_equal(expected: &Row, actual: &Row, shape: &Shape<'_>, precision: Precision) -> bool {
    shape
        .keys
        .iter()
        .all(|&c| values_equal(&expected[c], &actual[c], shape.kinds[c], precision))
}

fn values_equal(
    expected: &Option<String>,
    actual: &Option<String>,
    kind: Kind,
    precision: Precision,
) -> bool {
    let (expected, actual) = match (expected, actual) {
        (Some(expected), Some(actual)) => (expected, actual),
        (expected, actual) => return expected == actual,
    };
    if precision == Precision::Exact || expected == actual {
        return expected == actual;
    }
    if precision == Precision::Close {
        return kind != Kind::Exact
            && numbers(expected, actual).is_some_and(|(e, a)| {
                e.is_finite() && a.is_finite() && (a - e).abs() <= 1e-9 * e.abs().max(a.abs())
            });
    }
    let (expected, actual) = (expected.trim(), actual.trim());
    match kind {
        Kind::Exact => {
            expected == actual
                || matches!((decimal(expected), decimal(actual)), (Some(e), Some(a)) if e == a)
        }
        Kind::Sum => numbers(expected, actual).is_some_and(|(e, a)| (a - e).abs() <= 100.0 + 1e-6),
        Kind::Ratio => numbers(expected, actual)
            .is_some_and(|(e, a)| ((a * 100.0).round() / 100.0 - e).abs() <= 0.01 * e.abs() + 1e-9),
    }
}

fn numbers(expected: &str, actual: &str) -> Option<(f64, f64)> {
    Some((expected.parse().ok()?, actual.parse().ok()?))
}

/// A plain decimal number in a canonical form, so that `1995` and
/// `1995.0` are one value; `None` for anything else.
fn decimal(text: &str) -> Option<String> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let mut canonical = String::new();
    if negative && !(whole.is_empty() && fraction.is_empty()) {
        canonical.push('-');
    }
    canonical.push_str(if whole.is_empty() { "0" } else { whole });
    if !fraction.is_empty() {
        canonical.push('.');
        canonical.push_str(fraction);
    }
    Some(canonical)
}

#[cfg(test)]
mod tests {
    use super::Kind::*;
    use super::*;

    fn rows(text: &[&[&str]]) -> Vec<Row> {
        text.iter()
            .map(|row| {
                row.iter()
                    .map(|value| (*value != "NULL").then(|| value.to_string()))
                    .collect()
            })
            .collect()
    }

    const SUMS: Shape<'static> = Shape {
        kinds: &[Exact, Sum, Ratio, Exact],
        keys: &[0],
        limit: None,
        offset: 0,
    };

    #[test]
    fn the_specification_allows_its_tolerances() {
        let expected = rows(&[&["A", "1000.00", "25.52", "4"]]);
        let near = rows(&[&["A              ", "1099.9999", "25.5199", "4"]]);
        assert_eq!(
            compare(&expected, &near, &SUMS, Precision::Specification),
            Ok(())
        );
        // 25.52 within 1 %: 25.27 to 25.77 once rounded.
        let ratio = rows(&[&["A", "1000", "25.7649", "4"]]);
        assert_eq!(
            compare(&expected, &ratio, &SUMS, Precision::Specification),
            Ok(())
        );
        for wrong in [
            ["A", "1100.01", "25.52", "4"],
            ["A", "1000.00", "25.78", "4"],
            ["A", "1000.00", "25.52", "5"],
            ["B", "1000.00", "25.52", "4"],
        ] {
            let actual = rows(&[&wrong]);
            assert!(
                compare(&expected, &actual, &SUMS, Precision::Specification).is_err(),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn exact_values_compare_as_numbers() {
        let shape = Shape {
            kinds: &[Exact],
            keys: &[],
            limit: None,
            offset: 0,
        };
        let expected = rows(&[&["1995"], &["0.05"], &["-0"]]);
        let actual = rows(&[&["1995.0"], &["0.050"], &["0.00"]]);
        assert_eq!(
            compare(&expected, &actual, &shape, Precision::Specification),
            Ok(())
        );
        assert!(compare(&expected, &actual, &shape, Precision::Exact).is_err());
    }

    #[test]
    fn on_and_off_must_agree_byte_for_byte() {
        let expected = rows(&[&["A", "1000.0000", "25.52", "4"]]);
        let actual = rows(&[&["A", "1000.00", "25.52", "4"]]);
        assert_eq!(
            compare(&expected, &actual, &SUMS, Precision::Exact),
            Err(
                "row 1: expected [A | 1000.0000 | 25.52 | 4], got [A | 1000.00 | 25.52 | 4]".into()
            )
        );
        assert_eq!(
            compare(&expected, &expected, &SUMS, Precision::Exact),
            Ok(())
        );
    }

    #[test]
    fn rows_with_equal_keys_come_in_any_order() {
        let shape = Shape {
            kinds: &[Exact, Exact],
            keys: &[0],
            limit: None,
            offset: 0,
        };
        let expected = rows(&[&["1", "a"], &["2", "b"], &["2", "c"], &["3", "d"]]);
        let swapped = rows(&[&["1", "a"], &["2", "c"], &["2", "b"], &["3", "d"]]);
        assert_eq!(
            compare(&expected, &swapped, &shape, Precision::Exact),
            Ok(())
        );
        // Across different keys the order matters.
        let moved = rows(&[&["2", "b"], &["1", "a"], &["2", "c"], &["3", "d"]]);
        assert!(compare(&expected, &moved, &shape, Precision::Exact).is_err());
        let other = rows(&[&["1", "a"], &["2", "c"], &["2", "x"], &["3", "d"]]);
        assert_eq!(
            compare(&expected, &other, &shape, Precision::Exact),
            Err("row 2: expected [2 | b], got [2 | c]".into())
        );
    }

    #[test]
    fn past_the_limit_any_rows_of_the_run_come() {
        let shape = Shape {
            kinds: &[Exact, Exact],
            keys: &[0],
            limit: Some(3),
            offset: 0,
        };
        let expected = rows(&[&["1", "a"], &["2", "b"], &["2", "c"]]);
        let other = rows(&[&["1", "a"], &["2", "x"], &["2", "b"]]);
        assert_eq!(compare(&expected, &other, &shape, Precision::Exact), Ok(()));
        let wrong_key = rows(&[&["1", "a"], &["2", "x"], &["3", "b"]]);
        assert!(compare(&expected, &wrong_key, &shape, Precision::Exact).is_err());
        // Without the cut the run is compared whole.
        let uncut = Shape {
            limit: Some(4),
            ..shape
        };
        assert!(compare(&expected, &other, &uncut, Precision::Exact).is_err());
    }

    #[test]
    fn past_the_offset_any_rows_of_the_first_run_come() {
        let shape = Shape {
            kinds: &[Exact, Exact],
            keys: &[0],
            limit: Some(10),
            offset: 5,
        };
        let expected = rows(&[&["1", "a"], &["1", "b"], &["2", "c"], &["2", "d"]]);
        let other = rows(&[&["1", "x"], &["1", "a"], &["2", "d"], &["2", "c"]]);
        assert_eq!(compare(&expected, &other, &shape, Precision::Exact), Ok(()));
        // Only the first run: the next ones are whole.
        let wrong = rows(&[&["1", "a"], &["1", "b"], &["2", "c"], &["2", "x"]]);
        assert_eq!(
            compare(&expected, &wrong, &shape, Precision::Exact),
            Err("row 4: expected [2 | d], got [2 | x]".into())
        );
        let unskipped = Shape { offset: 0, ..shape };
        assert!(compare(&expected, &other, &unskipped, Precision::Exact).is_err());
        // Without keys every row is of one run, cut by the offset.
        let unsorted = Shape { keys: &[], ..shape };
        let any = rows(&[&["7", "y"], &["8", "z"], &["9", "w"], &["0", "v"]]);
        assert_eq!(
            compare(&expected, &any, &unsorted, Precision::Exact),
            Ok(())
        );
    }

    #[test]
    fn close_numbers_are_equal_in_their_columns_alone() {
        let shape = Shape {
            kinds: &[Ratio, Ratio, Exact],
            keys: &[],
            limit: None,
            offset: 0,
        };
        let expected = rows(&[&["1513.2345678901234568", "2.5e18", "a"]]);
        let near = rows(&[&["1513.2345678901235", "2500000000000000001", "a"]]);
        assert_eq!(compare(&expected, &near, &shape, Precision::Close), Ok(()));
        assert!(compare(&expected, &near, &shape, Precision::Exact).is_err());
        for wrong in [
            ["1513.23457", "2.5e18", "a"],
            ["1513.2345678901234568", "2.5e18", "b"],
            ["1513.2345678901234568", "NaN", "a"],
            ["Infinity", "2.5e18", "a"],
            ["-Infinity", "2.5e18", "a"],
        ] {
            let actual = rows(&[&wrong]);
            assert!(
                compare(&expected, &actual, &shape, Precision::Close).is_err(),
                "{wrong:?}"
            );
        }
        let zero = rows(&[&["0", "0", "a"]]);
        assert_eq!(compare(&zero, &zero, &shape, Precision::Close), Ok(()));
        let near_zero = rows(&[&["0.0000000001", "0", "a"]]);
        assert!(compare(&zero, &near_zero, &shape, Precision::Close).is_err());
        // An exact column is compared byte for byte, numbers or not.
        let ids = rows(&[&["1", "1", "435090932899640449"]]);
        let next = rows(&[&["1", "1", "435090932899640450"]]);
        assert!(compare(&ids, &next, &shape, Precision::Close).is_err());
        let phrase = rows(&[&["1", "1", "01"]]);
        let other = rows(&[&["1", "1", "1"]]);
        assert!(compare(&phrase, &other, &shape, Precision::Close).is_err());
    }

    #[test]
    fn close_keys_do_not_chain_into_one_run() {
        let shape = Shape {
            kinds: &[Ratio, Exact],
            keys: &[0],
            limit: Some(3),
            offset: 0,
        };
        // Each key is close to the next, not the first to the last: the
        // runs are of equal keys, and an answer equal to the reference
        // passes.
        let expected = rows(&[
            &["100.00000015", "a"],
            &["100.000000075", "b"],
            &["100", "c"],
        ]);
        assert_eq!(
            compare(&expected, &expected, &shape, Precision::Close),
            Ok(())
        );
    }

    #[test]
    fn counts_widths_and_nulls() {
        let shape = Shape {
            kinds: &[Exact, Sum],
            keys: &[],
            limit: None,
            offset: 0,
        };
        let expected = rows(&[&["a", "1"]]);
        assert_eq!(
            compare(&expected, &[], &shape, Precision::Exact),
            Err("0 rows, expected 1".into())
        );
        assert_eq!(
            compare(&expected, &rows(&[&["a"]]), &shape, Precision::Exact),
            Err("actual row has 1 columns, not 2".into())
        );
        let null = rows(&[&["a", "NULL"]]);
        assert!(compare(&expected, &null, &shape, Precision::Specification).is_err());
        assert_eq!(
            compare(&null, &null, &shape, Precision::Specification),
            Ok(())
        );
        assert_eq!(show(&null[0]), "[a | NULL]",);
    }

    #[test]
    fn decimals_are_canonical() {
        assert_eq!(decimal("007.50").as_deref(), Some("7.5"));
        assert_eq!(decimal("-0.00").as_deref(), Some("0"));
        assert_eq!(decimal(".5").as_deref(), Some("0.5"));
        assert_eq!(decimal("-12").as_deref(), Some("-12"));
        assert_eq!(decimal("1e3"), None);
        assert_eq!(decimal("BRAZIL"), None);
        assert_eq!(decimal(""), None);
    }
}
