//! The text kernels against references of another derivation: LIKE by
//! dynamic programming over the pattern, pieces over the string's
//! characters.

use tessera_kernels::text::{Chars, Like, Piece, char_count};

/// xorshift64*, fixed seed.
fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

#[test]
fn like_matches_as_dynamic_programming() {
    let mut state = 0x7E57_11CE_0000_0042;
    let alphabet = b"ab%";
    for _ in 0..200_000 {
        let plen = (random(&mut state) % 8) as usize;
        let slen = (random(&mut state) % 10) as usize;
        let pattern: Vec<u8> = (0..plen)
            .map(|_| alphabet[(random(&mut state) % 3) as usize])
            .collect();
        let string: Vec<u8> = (0..slen)
            .map(|_| alphabet[(random(&mut state) % 2) as usize])
            .collect();
        let like = Like::parse(&pattern).unwrap();
        assert_eq!(
            like.matches(&string),
            like_reference(&pattern, &string),
            "{} {}",
            String::from_utf8_lossy(&pattern),
            String::from_utf8_lossy(&string)
        );
    }
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

#[test]
fn pieces_match_the_characters() {
    let mut state = 0x0DDC_0FFE_E000_0007;
    let alphabet = [" ", "a", "é", "€", "𝄞"];
    for _ in 0..100_000 {
        let len = (random(&mut state) % 12) as usize;
        let string: String = (0..len)
            .map(|_| alphabet[(random(&mut state) % 5) as usize])
            .collect();
        let small = |state: &mut u64| (random(state) % 31) as i32 - 15;
        let piece = match random(&mut state) % 6 {
            0 => Piece::Substring {
                start: small(&mut state),
                length: random(&mut state)
                    .is_multiple_of(2)
                    .then(|| small(&mut state).abs()),
            },
            1 => Piece::Left(small(&mut state)),
            2 => Piece::Right(small(&mut state)),
            3 => Piece::Rtrim,
            4 => Piece::Ltrim,
            _ => Piece::Btrim,
        };
        let (from, to) = piece.bounds(string.as_bytes(), Chars::Utf8).unwrap();
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
}
