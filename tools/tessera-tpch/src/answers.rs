//! The published answers at SF 1, as the tpchgen crate carries them
//! (derived from TPC-H Tools): a header line, then rows of values padded
//! with spaces and divided by `|`.

use std::borrow::Cow;

use crate::compare::Row;

/// The answer to a query at SF 1, values trimmed; there is none at other
/// scale factors.
pub fn sf1(number: u8) -> Option<Vec<Row>> {
    let text = tpchgen::q_and_a::answers_sf1::answer(i32::from(number))?;
    Some(parse(text))
}

/// Our rows as the published answer shows them. The copy of Q11's answer
/// in tpchgen 3.0 lost the last two digits of every ps_partkey (129760 is
/// 1297 there, 9403 is 94; the values and their order agree), so ours
/// lose them too before the comparison.
pub fn as_published(number: u8, rows: &[Row]) -> Cow<'_, [Row]> {
    if number != 11 {
        return Cow::Borrowed(rows);
    }
    let truncate = |key: &Option<String>| {
        key.as_deref().map(|key| {
            key.trim()
                .parse::<u64>()
                .map_or_else(|_| key.to_string(), |key| (key / 100).to_string())
        })
    };
    Cow::Owned(
        rows.iter()
            .map(|row| {
                let mut row = row.clone();
                if let Some(key) = row.first_mut() {
                    *key = truncate(key);
                }
                row
            })
            .collect(),
    )
}

fn parse(text: &str) -> Vec<Row> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .skip(1)
        .map(|line| {
            line.split('|')
                .map(|value| Some(value.trim().to_string()))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::QUERIES;

    #[test]
    fn every_answer_parses_with_the_columns_of_its_query() {
        let rows = [
            4, 100, 10, 5, 5, 1, 4, 2, 175, 20, 1048, 2, 42, 1, 1, 18314, 1, 57, 1, 186, 100, 7,
        ];
        for query in &QUERIES {
            let answer = sf1(query.number).unwrap();
            assert_eq!(
                answer.len(),
                rows[usize::from(query.number) - 1],
                "{}",
                query.name()
            );
            for row in &answer {
                assert_eq!(row.len(), query.kinds.len(), "{}: {row:?}", query.name());
            }
        }
        assert_eq!(sf1(23), None);
    }

    #[test]
    fn q11_keys_lose_their_last_two_digits() {
        let ours = vec![
            vec![Some("129760".to_string()), Some("17538456.86".to_string())],
            vec![Some("9403".to_string()), Some("15451755.62".to_string())],
        ];
        let published = as_published(11, &ours);
        assert_eq!(published[0][0].as_deref(), Some("1297"));
        assert_eq!(published[1][0].as_deref(), Some("94"));
        assert_eq!(published[1][1].as_deref(), Some("15451755.62"));
        assert_eq!(sf1(11).unwrap()[0][0].as_deref(), Some("1297"));
        assert!(matches!(as_published(10, &ours), Cow::Borrowed(_)));
    }

    #[test]
    fn values_are_trimmed() {
        let q1 = sf1(1).unwrap();
        assert_eq!(q1[0][0].as_deref(), Some("A"));
        assert_eq!(q1[0][9].as_deref(), Some("1478493"));
        let q13 = sf1(13).unwrap();
        assert_eq!(q13[0][0].as_deref(), Some("0"));
    }
}
