//! Whether two answers to a query are the same: the answer of TPC-H
//! against ours by the precision rules of clause 2.1.3.5, and Tessera on
//! against off value for value.
//!
//! Rows are compared in their order, except that rows equal in the
//! columns of ORDER BY may come in any order, so such a run of rows is
//! compared as a multiset; and where the query's LIMIT cuts a run, the
//! rows that made it past the cut are any of the run's, so only their
//! sort columns are compared.

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
}

/// What the comparison needs to know of a query's answer.
#[derive(Debug, Clone, Copy)]
pub struct Shape<'a> {
    pub kinds: &'a [Kind],
    /// The output columns of ORDER BY, by position.
    pub keys: &'a [usize],
    pub limit: Option<usize>,
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
    let mut start = 0;
    while start < expected.len() {
        let mut end = start + 1;
        while end < expected.len()
            && keys_equal(&expected[end - 1], &expected[end], shape, precision)
        {
            end += 1;
        }
        if cut && end == expected.len() {
            // Past the cut any rows of the run may come: their keys only.
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
    if precision == Precision::Exact {
        return expected == actual;
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
    fn counts_widths_and_nulls() {
        let shape = Shape {
            kinds: &[Exact, Sum],
            keys: &[],
            limit: None,
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
