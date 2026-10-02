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
use tessera_kernels::text::{Chars, Like, Piece, char_count};
use tessera_testing::{edge, property};

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
