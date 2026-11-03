//! The proofs by Kani of the walk over one selection word, [`WordValues`],
//! which unsafe readers trust to give only rows in range, selected and
//! prepared. For any number of rows, word, selected and prepared bits,
//! through [`WordValues::checked`], the checks and the building of
//! [`WordValues::try_new`]:
//!
//! - a word is refused exactly when it calls for it, for the reason it
//!   calls for, stated apart from the code's checks, and a refusal comes
//!   with no call of the reader;
//! - the reader, checked inside each call, is called only for a row of
//!   the word that is selected, prepared and below `nrows`, each row
//!   after the last, and the row given is the row it was called for;
//! - the walk by `next` and the walk by `fold` give exactly the selected
//!   rows: the set of rows seen, one bit a row, equals the selection, and
//!   the reader is called once for each.
//!
//! The inputs are unbounded; the loops unwind once per row of a word and
//! once more. `try_new` itself only turns a refusal into an `anyhow`
//! error, which Kani 0.68 cannot compile, so the tests keep it.

use std::cell::Cell;

use super::{Refusal, WordValues};

/// What a reader checked in its calls: how many, and the last row.
#[derive(Default)]
struct Calls {
    count: Cell<u32>,
    last: Cell<Option<usize>>,
}

impl Calls {
    /// A call for `row`: of the word, below `nrows`, selected, prepared
    /// and after the last one.
    fn check(&self, row: usize, nrows: usize, word: usize, selected: u64, prepared: u64) {
        assert!(row < nrows);
        assert!(row / 64 == word);
        let bit = row % 64;
        assert!(selected >> bit & 1 == 1);
        assert!(prepared >> bit & 1 == 1);
        if let Some(last) = self.last.get() {
            assert!(row > last);
        }
        self.last.set(Some(row));
        self.count.set(self.count.get() + 1);
    }
}

/// The refusal a word calls for, stated apart from the code's checks: a
/// word whose first row is past the rows, a selected row past the rows,
/// a selected row that is not prepared; none for a word to walk.
fn expected(nrows: usize, word: usize, selected: u64, prepared: u64) -> Option<Refusal> {
    let Some(base) = word.checked_mul(64).filter(|&base| base < nrows) else {
        return Some(Refusal::OutOfBounds);
    };
    let rows_in_word = nrows - base;
    if rows_in_word < 64 && selected >> rows_in_word != 0 {
        return Some(Refusal::Padding);
    }
    (selected & !prepared != 0).then_some(Refusal::Unprepared)
}

/// The refusal of a word is the one it calls for, and each is met.
fn check_refusal(refusal: Refusal, nrows: usize, word: usize, selected: u64, prepared: u64) {
    assert!(expected(nrows, word, selected, prepared) == Some(refusal));
    kani::cover!(refusal == Refusal::OutOfBounds);
    kani::cover!(refusal == Refusal::Padding);
    kani::cover!(refusal == Refusal::Unprepared);
}

#[kani::proof]
#[kani::unwind(66)]
fn the_walk_by_next_gives_the_selected_rows() {
    let (nrows, word, selected, prepared) = (kani::any(), kani::any(), kani::any(), kani::any());
    let calls = Calls::default();
    let read = |row: usize| {
        calls.check(row, nrows, word, selected, prepared);
        Some(row)
    };
    match WordValues::checked(nrows, word, selected, prepared, read, |refusal| refusal) {
        Err(refusal) => {
            assert!(calls.count.get() == 0);
            check_refusal(refusal, nrows, word, selected, prepared);
        }
        Ok(values) => {
            assert!(expected(nrows, word, selected, prepared).is_none());
            let mut seen = 0u64;
            let mut given = 0u32;
            for (row, value) in values {
                assert!(value == Some(row));
                given += 1;
                assert!(calls.count.get() == given);
                let bit = 1u64 << (row % 64);
                assert!(seen & bit == 0);
                seen |= bit;
            }
            assert!(seen == selected);
            kani::cover!(selected == u64::MAX);
            kani::cover!(selected != u64::MAX && selected != 0);
            kani::cover!(selected == 0);
        }
    }
}

#[kani::proof]
#[kani::unwind(66)]
fn the_walk_by_fold_gives_the_selected_rows() {
    let (nrows, word, selected, prepared) = (kani::any(), kani::any(), kani::any(), kani::any());
    let calls = Calls::default();
    let read = |row: usize| {
        calls.check(row, nrows, word, selected, prepared);
        Some(row)
    };
    let made = WordValues::checked(nrows, word, selected, prepared, read, |refusal| refusal);
    if let Err(refusal) = made {
        assert!(calls.count.get() == 0);
        assert!(expected(nrows, word, selected, prepared) == Some(refusal));
    }
    if let Ok(values) = made {
        assert!(expected(nrows, word, selected, prepared).is_none());
        let (seen, given) = values.fold((0u64, 0u32), |(seen, given), (row, value)| {
            assert!(value == Some(row));
            assert!(calls.count.get() == given + 1);
            let bit = 1u64 << (row % 64);
            assert!(seen & bit == 0);
            (seen | bit, given + 1)
        });
        assert!(seen == selected);
        assert!(calls.count.get() == given);
        kani::cover!(selected == u64::MAX);
        kani::cover!(selected != u64::MAX && selected != 0);
    }
}
