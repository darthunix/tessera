#![forbid(unsafe_code)]

use std::cell::Cell;

use anyhow::{Context, Result};
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView, WordValues};

#[test]
fn reader_supports_wide_values_and_preserves_inherent_get() {
    let values = [i64::MIN, 0, i64::MAX];
    let column = ColumnView::try_new(&values, None).unwrap();
    assert!(std::ptr::eq(column.get(0).unwrap().unwrap(), &values[0]));
    assert_eq!(ColumnReader::get(&column, 0).unwrap(), Some(i64::MIN));
    assert!(ColumnReader::get(&column, 3).is_err());
    let selected = RowMaskView::try_from_bytes(3, &[0b1010], 1).unwrap();
    let result = column
        .try_fold_selected(&selected, Vec::new(), |mut values, row, value| {
            values.push((row, value));
            Ok(values)
        })
        .unwrap();
    assert_eq!(result, [(0, Some(i64::MIN)), (2, Some(i64::MAX))]);
}

struct Text<'a>(&'a [String]);

impl<'a> ColumnReader for Text<'a> {
    type Value = &'a str;

    fn nrows(&self) -> usize {
        self.0.len()
    }

    fn get(&self, row: usize) -> Result<Option<Self::Value>> {
        Ok(Some(
            self.0.get(row).context("row is out of bounds")?.as_str(),
        ))
    }

    fn word_values(
        &self,
        index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<Self::Value>)> + '_> {
        let strings = self.0;
        WordValues::try_new(
            self.nrows(),
            index,
            selected,
            u64::MAX,
            move |row: usize| Some(strings[row].as_str()),
        )
    }
}

#[test]
fn reader_borrows_non_static_text_without_copying() {
    let strings = vec![String::from("first"), String::from("second")];
    let column = Text(&strings);
    let rows = RowMaskView::try_new(2, &[3]).unwrap();
    let visited = column
        .try_fold_selected(&rows, 0, |visited, row, value| {
            let value = value.unwrap();
            assert_eq!(value, strings[row]);
            assert_eq!(value.as_ptr(), strings[row].as_ptr());
            Ok(visited + 1)
        })
        .unwrap();
    assert_eq!(visited, 2);
}

struct NotCopyOrClone(i32);

#[test]
fn word_values_has_no_copy_or_clone_bound() {
    let mut values = WordValues::try_new(1, 0, 1, 1, |_| Some(NotCopyOrClone(42))).unwrap();
    assert_eq!(values.next().unwrap().1.unwrap().0, 42);
    assert!(values.next().is_none());
    assert!(values.next().is_none());
}

struct Tracked<'a> {
    nrows: usize,
    prepared: &'a [u64],
    words: Cell<usize>,
    reads: Cell<usize>,
}

impl ColumnReader for Tracked<'_> {
    type Value = NotCopyOrClone;

    fn nrows(&self) -> usize {
        self.nrows
    }

    fn get(&self, _: usize) -> Result<Option<Self::Value>> {
        panic!("selected iteration must not call get")
    }

    fn word_values(
        &self,
        index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<Self::Value>)> + '_> {
        self.words.set(self.words.get() + 1);
        WordValues::try_new(
            self.nrows,
            index,
            selected,
            self.prepared[index],
            move |row| {
                self.reads.set(self.reads.get() + 1);
                Some(NotCopyOrClone(row as i32))
            },
        )
    }
}

#[test]
fn selected_fold_checks_each_nonempty_word_once_before_reading() {
    for invalid_word in 0..3 {
        let mut prepared = [u64::MAX; 3];
        prepared[invalid_word] &= !(1 << 10);
        let column = Tracked {
            nrows: 192,
            prepared: &prepared,
            words: Cell::new(0),
            reads: Cell::new(0),
        };
        let rows = RowMaskView::try_new(192, &[u64::MAX; 3]).unwrap();
        let mut visited = Vec::new();
        let result = column.try_fold_selected(&rows, (), |(), row, value| {
            assert_eq!(value.unwrap().0, row as i32);
            visited.push(row);
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(visited, (0..invalid_word * 64).collect::<Vec<_>>());
        assert_eq!(column.words.get(), invalid_word + 1);
        assert_eq!(column.reads.get(), invalid_word * 64);
    }
    let column = Tracked {
        nrows: 192,
        prepared: &[0, 1, 0],
        words: Cell::new(0),
        reads: Cell::new(0),
    };
    let rows = RowMaskView::try_new(192, &[0, 1, 0]).unwrap();
    let count = column
        .try_fold_selected(&rows, 0, |count, _, _| Ok(count + 1))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(column.words.get(), 1);
    assert_eq!(column.reads.get(), 1);
    let wrong = RowMaskView::try_new(1, &[1]).unwrap();
    assert!(
        column
            .try_fold_selected(&wrong, (), |(), _, _| Ok(()))
            .is_err()
    );
    assert_eq!(column.words.get(), 1);
}

#[test]
fn word_validation_never_calls_reader_for_invalid_requests() {
    for (nrows, index, selected, prepared) in [
        (0, 0, 0, 0),
        (1, 1, 0, 0),
        (1, usize::MAX, 1, 1),
        (1, 0, 2, 2),
        (64, 1, 0, 0),
        (65, 1, 2, 2),
        (128, 2, 0, 0),
        (64, 0, 3, 1),
        (usize::MAX, usize::MAX / 64, 1 << 63, u64::MAX),
        (usize::MAX, usize::MAX / 64 + 1, 0, 0),
    ] {
        let calls = Cell::new(0);
        let result = WordValues::try_new(nrows, index, selected, prepared, |_| {
            calls.set(calls.get() + 1);
            Some(1)
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 0);
    }
    let mut empty =
        WordValues::try_new(1, 0, 0, 0, |_| panic!("empty word must not read")).unwrap();
    assert_eq!(Iterator::next(&mut empty), None::<(usize, Option<()>)>);
    let last = usize::MAX / 64;
    let mut extreme = WordValues::try_new(usize::MAX, last, 1 << 62, u64::MAX, Some).unwrap();
    assert_eq!(extreme.next(), Some((usize::MAX - 1, Some(usize::MAX - 1))));
}

#[test]
fn consumer_error_stops_the_fold_before_further_reads() {
    for stop in [0, 1, 63, 64, 65] {
        let column = Tracked {
            nrows: 192,
            prepared: &[u64::MAX; 3],
            words: Cell::new(0),
            reads: Cell::new(0),
        };
        let rows = RowMaskView::try_new(192, &[u64::MAX; 3]).unwrap();
        let mut next = 0;
        let result = column.try_fold_selected(&rows, (), |(), row, value| {
            assert_eq!(row, next);
            assert_eq!(value.unwrap().0, row as i32);
            next += 1;
            anyhow::ensure!(row != stop, "consumer stopped");
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(next, stop + 1);
        assert_eq!(column.words.get(), stop / 64 + 1);
        assert_eq!(column.reads.get(), stop + 1);
    }
}

#[test]
fn word_validation_checks_every_tail_bit() {
    for nrows in 1_usize..=129 {
        for index in 0..nrows.div_ceil(64) {
            for bit in 0..64 {
                let row = index * 64 + bit;
                let result = WordValues::try_new(nrows, index, 1 << bit, u64::MAX, Some);
                if row < nrows {
                    assert_eq!(result.unwrap().next(), Some((row, Some(row))));
                } else {
                    assert!(result.is_err());
                }
            }
        }
    }
}

#[test]
fn copied_word_allows_mutating_active_mask_during_iteration() {
    let values = [10, 20, 30];
    let column = ColumnView::try_new(&values, None).unwrap();
    let mut words = [7];
    let mut rows = RowMask::try_new(3, &mut words).unwrap();
    let selected = rows.as_view().word(0).unwrap();
    for (row, _) in column.word_values(0, selected).unwrap() {
        rows.clear(row);
    }
    assert_eq!(rows.as_view().selected_count(), 0);
}
