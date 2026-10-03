//! The text kernels against references of another derivation: LIKE by
//! dynamic programming over the pattern, pieces over the string's
//! characters.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use anyhow::Result;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use std::mem::MaybeUninit;

use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::text::{
    self, Bounds, Chars, Length, Like, Piece, Strings, Text, char_count, lengths, like,
};
use tessera_testing::{edge, property};

/// Rows of strings for the batch functions: a NULL in every seventh, a
/// value not in place in every eleventh, and strings of `a` and `b`.
struct Rows(Vec<Option<Option<String>>>);

impl Rows {
    fn new(nrows: usize) -> Self {
        Self(
            (0..nrows)
                .map(|row| match row {
                    _ if row % 7 == 3 => None,
                    _ if row % 11 == 5 => Some(None),
                    _ => Some(Some(
                        (0..row % 9)
                            .map(|at| if (row + at) % 3 == 0 { 'b' } else { 'a' })
                            .collect(),
                    )),
                })
                .collect(),
        )
    }
}

impl Strings for Rows {
    fn get(&self, row: usize) -> Text<'_> {
        match &self.0[row] {
            None => Text::Null,
            Some(None) => Text::Other,
            Some(Some(string)) => Text::Bytes(string.as_bytes()),
        }
    }
}

/// The selected rows: all but every fifth, over several words of a mask.
fn selected(nrows: usize) -> Vec<u64> {
    let mut words = vec![0_u64; nrows.div_ceil(64)];
    for row in (0..nrows).filter(|row| row % 5 != 2) {
        words[row / 64] |= 1 << (row % 64);
    }
    words
}

fn bits(words: &[u64], nrows: usize) -> Vec<bool> {
    (0..nrows)
        .map(|row| words[row / 64] >> (row % 64) & 1 == 1)
        .collect()
}

/// LIKE, a length and a piece of 200 rows, row by row as a model says: a
/// NULL leaves the rows, a value not in place goes to the rest, a string
/// is kept by its match, its length and its piece written at its row.
#[test]
fn the_batch_functions_answer_each_selected_row() -> Result<()> {
    let nrows = 200;
    let source = Rows::new(nrows);
    let selected = selected(nrows);
    let chosen = bits(&selected, nrows);
    let pattern = Like::parse(b"%ab%").unwrap();
    let mut kept = selected.clone();
    let mut rest = vec![0_u64; kept.len()];
    like(
        &source,
        &pattern,
        false,
        &mut RowMask::try_new(nrows, &mut kept)?,
        &mut RowMask::try_new(nrows, &mut rest)?,
    )?;
    let (kept, rest) = (bits(&kept, nrows), bits(&rest, nrows));
    for row in 0..nrows {
        let (keep, other) = match (&source.0[row], chosen[row]) {
            (_, false) | (None, true) => (false, false),
            (Some(None), true) => (false, true),
            (Some(Some(string)), true) => (string.contains("ab"), false),
        };
        assert_eq!((kept[row], rest[row]), (keep, other), "LIKE of row {row}");
    }
    let view = RowMaskView::try_new(nrows, &selected)?;
    let mut values = vec![MaybeUninit::new(-1); nrows];
    let mut non_nulls = vec![0_u64; selected.len()];
    let mut rest = vec![0_u64; selected.len()];
    lengths(
        Length::Octets,
        Chars::Utf8,
        &source,
        view,
        &mut values,
        &mut RowMask::try_new(nrows, &mut non_nulls)?,
        &mut RowMask::try_new(nrows, &mut rest)?,
    )?;
    let piece = Piece::Substring {
        start: 2,
        length: Some(3),
    };
    let mut starts = vec![MaybeUninit::new(-1); nrows];
    let mut taken = vec![MaybeUninit::new(-1); nrows];
    let mut piece_non_nulls = vec![0_u64; selected.len()];
    let mut piece_rest = vec![0_u64; selected.len()];
    text::pieces(
        piece,
        Chars::Utf8,
        &source,
        view,
        &mut Bounds {
            starts: &mut starts,
            lengths: &mut taken,
            non_nulls: RowMask::try_new(nrows, &mut piece_non_nulls)?,
            rest: RowMask::try_new(nrows, &mut piece_rest)?,
        },
    )?;
    let (non_nulls, rest) = (bits(&non_nulls, nrows), bits(&rest, nrows));
    let (piece_non_nulls, piece_rest) = (bits(&piece_non_nulls, nrows), bits(&piece_rest, nrows));
    for row in 0..nrows {
        let present = chosen[row] && source.0[row].is_some();
        let other = chosen[row] && matches!(source.0[row], Some(None));
        assert_eq!(
            (non_nulls[row], rest[row]),
            (present, other),
            "length of row {row}"
        );
        assert_eq!(
            (piece_non_nulls[row], piece_rest[row]),
            (present, other),
            "piece of row {row}"
        );
        if let (true, Some(Some(string))) = (chosen[row], &source.0[row]) {
            // SAFETY: written for every selected row with a string in place.
            let (length, start, take) = unsafe {
                (
                    values[row].assume_init(),
                    starts[row].assume_init(),
                    taken[row].assume_init(),
                )
            };
            assert_eq!(length as usize, string.len(), "length of row {row}");
            let from = string.len().min(1);
            let to = string.len().min(4);
            assert_eq!(
                (start as usize, take as usize),
                (from, to - from),
                "piece of row {row}"
            );
        }
    }
    Ok(())
}

/// LIKE of literals and `%` by dynamic programming: `matched[i][j]` when
/// the first `i` pattern bytes match the first `j` string bytes.
fn like_reference(pattern: &[u8], string: &[u8]) -> bool {
    let mut matched = vec![vec![false; string.len() + 1]; pattern.len() + 1];
    matched[0][0] = true;
    for i in 1..=pattern.len() {
        for j in 0..=string.len() {
            matched[i][j] = if pattern[i - 1] == b'%' {
                matched[i - 1][j] || (j > 0 && matched[i][j - 1])
            } else {
                j > 0 && matched[i - 1][j - 1] && pattern[i - 1] == string[j - 1]
            };
        }
    }
    matched[pattern.len()][string.len()]
}

/// Patterns of `a`, `b` and `%` against strings of `a` and `b`, short
/// enough that every way to split a match occurs.
#[test]
fn like_matches_as_dynamic_programming() {
    let pair = (
        vec(select(b"ab%".to_vec()), 0..8),
        vec(select(b"ab".to_vec()), 0..10),
    );
    property(vec(pair, 0..64), |pairs| -> Result<()> {
        for (pattern, string) in pairs {
            let like = Like::parse(&pattern).unwrap();
            assert_eq!(
                like.matches(&string),
                like_reference(&pattern, &string),
                "{} {}",
                String::from_utf8_lossy(&pattern),
                String::from_utf8_lossy(&string)
            );
        }
        Ok(())
    });
}

/// A piece by the string's characters, as the core's functions describe it.
fn piece_reference(piece: Piece, string: &str) -> String {
    let chars: Vec<char> = string.chars().collect();
    let count = chars.len() as i64;
    let slice = |from: i64, to: i64| -> String {
        let from = from.clamp(0, count) as usize;
        let to = to.clamp(0, count) as usize;
        if from >= to {
            String::new()
        } else {
            chars[from..to].iter().collect()
        }
    };
    match piece {
        Piece::Substring { start, length } => {
            let from = i64::from(start.max(1)) - 1;
            match length {
                None => slice(from, count),
                Some(length) => slice(from, i64::from(start) + i64::from(length) - 1),
            }
        }
        Piece::Left(n) if n >= 0 => slice(0, i64::from(n)),
        Piece::Left(n) => slice(0, count + i64::from(n)),
        Piece::Right(n) if n >= 0 => slice(count - i64::from(n), count),
        Piece::Right(n) => slice(-i64::from(n), count),
        Piece::Rtrim => string.trim_end_matches(' ').to_owned(),
        Piece::Ltrim => string.trim_start_matches(' ').to_owned(),
        Piece::Btrim => string.trim_matches(' ').to_owned(),
    }
}

/// A count or position of a piece: small, around the string's length, or
/// an edge of int4.
fn count() -> BoxedStrategy<i32> {
    prop_oneof![3 => -15..=15, 1 => edge::<i32>()].boxed()
}

fn pieces() -> impl Strategy<Value = Piece> {
    prop_oneof![
        (count(), proptest::option::of(count())).prop_map(|(start, length)| Piece::Substring {
            start,
            length: length.map(i32::saturating_abs)
        }),
        count().prop_map(Piece::Left),
        count().prop_map(Piece::Right),
        Just(Piece::Rtrim),
        Just(Piece::Ltrim),
        Just(Piece::Btrim),
    ]
}

/// Strings of spaces and characters of one to four UTF-8 bytes.
fn strings() -> impl Strategy<Value = String> {
    vec(select(vec![" ", "a", "é", "€", "𝄞"]), 0..12).prop_map(|chars| chars.concat())
}

#[test]
fn pieces_match_the_characters() {
    property(vec((pieces(), strings()), 0..64), |cases| -> Result<()> {
        for (piece, string) in cases {
            let (from, to) = piece.bounds(string.as_bytes(), Chars::Utf8)?;
            assert_eq!(
                &string.as_bytes()[from..to],
                piece_reference(piece, &string).as_bytes(),
                "{piece:?} {string:?}"
            );
            assert_eq!(
                char_count(string.as_bytes(), Chars::Utf8),
                string.chars().count() as i64
            );
        }
        Ok(())
    });
}
