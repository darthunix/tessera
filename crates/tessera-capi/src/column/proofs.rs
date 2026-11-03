//! The proofs by Kani of the walk of the readers' folds, [`walk_words`],
//! under which they read values with `get_unchecked`. With any selection,
//! readiness and NULL bits, either mask of readiness or NULLs absent, and
//! any fold step failing:
//!
//! - the reader is called only for a selected, prepared row below the
//!   column's rows, each once, in increasing order, with the NULL bits of
//!   its word, and the fold step gets the row read;
//! - a selection of another row count reads nothing;
//! - a word with an unprepared selected row is refused before any of its
//!   rows is read, and nothing after it is read; the words before it were
//!   read whole;
//! - a fold step that fails ends the walk, and no row after it is read;
//! - with no refusal and no failure, every selected row is read.
//!
//! Bounds: a column of one word, any of its bits, either mask absent; and
//! of two words, the first word's selection empty or full and its rows
//! prepared, no mask of NULLs: 70 rows, the partial second word's bits
//! any, or 128, the second word wholly selected and its readiness any.
//! The walk over words runs one body for each word, so the second word,
//! after a first that was skipped or read whole, is where the walk goes
//! on, and refuses or fails after reads; what varies in a word is proven
//! by the harness of one word. The errors are the proofs'
//! own: `try_fold_words` only builds `anyhow` ones with the same reasons,
//! which Kani 0.68 cannot compile.

use std::cell::Cell;

use tessera_core::RowMaskView;

use super::{WalkRefusal, walk_words};

/// How a walk ended without its fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stop {
    Refused(WalkRefusal),
    Failed,
}

/// Whether row `row` is set in `mask`, a mask of at least its rows.
fn has(mask: &RowMaskView<'_>, row: usize) -> bool {
    mask.word(row / 64) >> (row % 64) & 1 == 1
}

/// How a walk went: its end, the reads, and the selection of the first
/// and the last word.
struct Walked {
    end: Result<u32, Stop>,
    reads: u32,
    first: u64,
    last: u64,
}

/// A walk over `nrows` rows, fixed by the harness, with any bits. With
/// `first` given, a walk of two words: the first word's selection is one
/// of the two, its rows all prepared and no mask of NULLs, so that the
/// bits that vary are those of the second word; `second`, when given, is
/// the second word's selection.
fn walk(nrows: usize, first: Option<[u64; 2]>, second: Option<u64>) -> Option<Walked> {
    let words = nrows.div_ceil(64);
    let (mut selection, mut ready, valid): ([u64; 2], [u64; 2], [u64; 2]) =
        (kani::any(), kani::any(), kani::any());
    if let Some(choices) = first {
        selection[0] = if kani::any() { choices[0] } else { choices[1] };
        ready[0] = u64::MAX;
    }
    if let Some(bits) = second {
        selection[1] = bits;
    }
    let rows = RowMaskView::try_new(nrows, &selection[..words]).ok()?;
    let prepared = if kani::any() {
        None
    } else {
        Some(RowMaskView::try_new(nrows, &ready[..words]).ok()?)
    };
    let non_nulls = if first.is_some() || kani::any() {
        None
    } else {
        Some(RowMaskView::try_new(nrows, &valid[..words]).ok()?)
    };
    let fail_at: u32 = kani::any();
    let reads = Cell::new(0u32);
    let last = Cell::new(None::<usize>);
    let read = |row: usize, bits: u64| {
        assert!(row < nrows);
        assert!(has(&rows, row));
        assert!(prepared.is_none_or(|mask| has(&mask, row)));
        assert!(bits == non_nulls.map_or(u64::MAX, |mask| mask.word(row / 64)));
        if let Some(last) = last.get() {
            assert!(row > last);
        }
        last.set(Some(row));
        reads.set(reads.get() + 1);
        Some(row)
    };
    let mut seen = [0u64; 2];
    let result = walk_words(
        nrows,
        &rows,
        prepared,
        non_nulls,
        read,
        0u32,
        |given, row, value| {
            assert!(value == Some(row));
            assert!(reads.get() == given + 1);
            seen[row / 64] |= 1 << (row % 64);
            if given == fail_at {
                return Err(Stop::Failed);
            }
            Ok(given + 1)
        },
        Stop::Refused,
    );
    // The first word with a selected row that is not prepared, if any.
    let refused = (0..words)
        .find(|&word| rows.word(word) & !prepared.map_or(u64::MAX, |mask| mask.word(word)) != 0);
    match result {
        Ok(count) => {
            assert!(refused.is_none());
            assert!(reads.get() == count);
            for (word, bits) in seen.iter().enumerate().take(words) {
                assert!(*bits == rows.word(word));
            }
        }
        Err(Stop::Refused(WalkRefusal::Unprepared)) => {
            let Some(word) = refused else { panic!() };
            assert!(last.get().is_none_or(|row| row < word * 64));
            // The words before it were read whole, as no step failed.
            for (before, bits) in seen.iter().enumerate().take(word) {
                assert!(*bits == rows.word(before));
            }
        }
        Err(Stop::Refused(WalkRefusal::Rows)) => panic!(),
        Err(Stop::Failed) => assert!(reads.get() == fail_at + 1),
    }
    Some(Walked {
        end: result,
        reads: reads.get(),
        first: rows.word(0),
        last: rows.word(words - 1),
    })
}

#[kani::proof]
#[kani::unwind(66)]
fn one_word_is_read_as_selected() {
    if let Some(walked) = walk(64, None, None) {
        kani::cover!(walked.end.is_ok() && walked.first == u64::MAX);
        kani::cover!(walked.end.is_ok() && walked.first != u64::MAX && walked.first != 0);
        kani::cover!(walked.end == Err(Stop::Refused(WalkRefusal::Unprepared)));
        kani::cover!(walked.end == Err(Stop::Failed) && walked.reads > 1);
    }
}

#[kani::proof]
#[kani::unwind(66)]
fn a_full_second_word_is_read_as_selected() {
    if let Some(walked) = walk(128, Some([0, u64::MAX]), Some(u64::MAX)) {
        kani::cover!(walked.end.is_ok() && walked.first == u64::MAX);
        kani::cover!(walked.end.is_ok() && walked.first == 0);
        kani::cover!(
            walked.end == Err(Stop::Refused(WalkRefusal::Unprepared)) && walked.reads == 64
        );
        kani::cover!(walked.end == Err(Stop::Failed) && walked.reads > 64);
    }
}

#[kani::proof]
#[kani::unwind(66)]
fn a_partial_last_word_is_read_as_selected() {
    if let Some(walked) = walk(70, Some([0, u64::MAX]), None) {
        kani::cover!(walked.end.is_ok() && walked.last != 0);
        kani::cover!(
            walked.end == Err(Stop::Refused(WalkRefusal::Unprepared)) && walked.reads == 64
        );
        kani::cover!(walked.end == Err(Stop::Failed) && walked.reads > 64);
    }
}

// The refusal comes before the loop over words, which no unwinding then
// reaches.
#[kani::proof]
#[kani::unwind(1)]
fn a_selection_of_other_rows_reads_nothing() {
    let (nrows, other): (usize, usize) = (kani::any(), kani::any());
    kani::assume(nrows <= 128 && other <= 128 && nrows != other);
    let words = [0u64; 2];
    let Ok(rows) = RowMaskView::try_new(other, &words[..other.div_ceil(64)]) else {
        panic!()
    };
    let result = walk_words(
        nrows,
        &rows,
        None,
        None,
        |_: usize, _: u64| -> Option<u8> { panic!() },
        (),
        |(), _, _| Ok::<(), Stop>(()),
        Stop::Refused,
    );
    assert!(result == Err(Stop::Refused(WalkRefusal::Rows)));
}
