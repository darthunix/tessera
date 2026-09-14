use anyhow::{Result, ensure};

pub(crate) fn word_count(nrows: usize) -> usize {
    nrows.div_ceil(64)
}

pub(crate) fn validate_words(nrows: usize, words: &[u64]) -> Result<()> {
    let expected = word_count(nrows);
    ensure!(
        words.len() == expected,
        "expected {expected} bitmap words, got {}",
        words.len()
    );
    let tail = nrows % 64;
    ensure!(
        tail == 0 || words[expected - 1] >> tail == 0,
        "bitmap has set bits beyond its physical row count"
    );
    Ok(())
}

#[inline]
pub(crate) fn validate_row(row: usize, nrows: usize) -> Result<()> {
    ensure!(
        row < nrows,
        "physical row {row} is out of bounds for {nrows} rows"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::word_count;

    #[test]
    fn word_count_does_not_overflow() {
        for (nrows, expected) in [
            (0, 0),
            (1, 1),
            (63, 1),
            (64, 1),
            (65, 2),
            (usize::MAX - 63, usize::MAX / 64),
            (usize::MAX - 62, usize::MAX / 64 + 1),
            (usize::MAX, usize::MAX / 64 + 1),
        ] {
            assert_eq!(word_count(nrows), expected);
        }
    }
}
