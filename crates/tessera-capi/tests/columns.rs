use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::{ColumnReader, ColumnView, RowMaskView};

fn words_for(flags: &[bool]) -> Vec<u64> {
    let mut words = vec![0; flags.len().div_ceil(64)];
    for (row, &flag) in flags.iter().enumerate() {
        if flag {
            words[row / 64] |= 1 << (row % 64);
        }
    }
    words
}

fn collect<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Vec<(usize, Option<i32>)> {
    column
        .try_fold_selected(rows, Vec::new(), |mut values, row, value| {
            values.push((row, value));
            Ok(values)
        })
        .unwrap()
}

#[test]
fn representations_agree_with_uninitialized_gaps() {
    for nrows in [0, 1, 7, 8, 9, 63, 64, 65, 70, 127, 128, 129, 257, 1024] {
        for kind in 0..4 {
            let values: Vec<_> = (0..nrows)
                .map(|row| [i32::MIN, -42, -1, 0, 1, 42, i32::MAX][row % 7])
                .collect();
            let ready: Vec<_> = (0..nrows).map(|row| kind == 0 || row % 3 != 0).collect();
            let non_null: Vec<_> = (0..nrows)
                .map(|row| kind == 0 || (kind != 1 && row % 5 != 0))
                .collect();
            let selected: Vec<_> = (0..nrows)
                .map(|row| ready[row] && (kind != 3 || row % 64 == 1))
                .collect();
            let mut dense_values = vec![MaybeUninit::uninit(); nrows];
            let mut datum_values = vec![MaybeUninit::uninit(); nrows];
            let mut isnull = vec![MaybeUninit::uninit(); nrows];
            for row in 0..nrows {
                if ready[row] {
                    isnull[row].write(!non_null[row]);
                    // NULL rows hold an initialized value of no meaning.
                    let (dense, datum) = if non_null[row] {
                        (values[row], values[row] as u64)
                    } else {
                        (0x5a5a_5a5a, 0xdead_beef_dead_beef)
                    };
                    dense_values[row].write(dense);
                    datum_values[row].write(datum);
                }
            }
            let ready_words = words_for(&ready);
            let non_null_words = words_for(&non_null);
            let selected_words = words_for(&selected);
            let prepared = RowMaskView::try_new(nrows, &ready_words).unwrap();
            let non_nulls = RowMaskView::try_new(nrows, &non_null_words).unwrap();
            let selection = RowMaskView::try_new(nrows, &selected_words).unwrap();
            let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
            // SAFETY: every prepared position has a value and a flag; only
            // unprepared gaps are uninitialized.
            let dense = unsafe {
                DenseInt32Column::try_new(&dense_values, Some(non_nulls), Some(prepared))
            }
            .unwrap();
            // SAFETY: the same readiness and initialization guarantees hold.
            let datum =
                unsafe { DatumInt32Column::try_new(&datum_values, &isnull, Some(prepared)) }
                    .unwrap();
            let expected = collect(&column, &selection);
            assert_eq!(collect(&dense, &selection), expected);
            assert_eq!(collect(&datum, &selection), expected);
            assert_fold_stops(&dense, &selection, &expected);
            assert_fold_stops(&datum, &selection, &expected);
            for (row, &is_ready) in ready.iter().enumerate() {
                if is_ready {
                    let value = ColumnReader::get(&column, row).unwrap();
                    assert_eq!(dense.get(row).unwrap(), value);
                    assert_eq!(datum.get(row).unwrap(), value);
                } else {
                    assert!(dense.get(row).is_err());
                    assert!(datum.get(row).is_err());
                }
            }
            for row in [nrows, usize::MAX] {
                assert!(dense.get(row).is_err());
                assert!(datum.get(row).is_err());
            }
        }
    }
}

fn assert_reader_error<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    prior_rows: usize,
) {
    let mut visited = Vec::new();
    let result: Result<()> = column.try_fold_selected(rows, (), |(), row, _| {
        visited.push(row);
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(
        visited.len(),
        prior_rows,
        "no values may follow a readiness error"
    );
    assert!(visited.windows(2).all(|pair| pair[0] < pair[1]));
}

// The fold yields exactly `expected`, and a consumer error after any prefix
// stops it with exactly that prefix consumed, including word boundaries.
fn assert_fold_stops<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
    expected: &[(usize, Option<i32>)],
) {
    assert_eq!(collect(column, rows), expected);
    for stop in [0, 1, 62, 63, 64, 65, 126, 127, 128, 129, 190, 191, 192] {
        if stop >= expected.len() {
            continue;
        }
        let mut consumed = Vec::new();
        let result = column.try_fold_selected(rows, (), |(), row, value| {
            consumed.push((row, value));
            anyhow::ensure!(consumed.len() <= stop, "consumer stopped");
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(consumed, expected[..=stop]);
    }
}

#[test]
fn unprepared_flags_and_values_are_never_read() {
    let mut dense_values = [MaybeUninit::uninit(); 129];
    let mut datum_values = [MaybeUninit::uninit(); 129];
    let mut isnull = [MaybeUninit::uninit(); 129];
    for row in 0..64 {
        isnull[row].write(true);
        dense_values[row].write(-1);
        datum_values[row].write(u64::MAX);
    }
    let prepared = RowMaskView::try_new(129, &[u64::MAX, 0, 0]).unwrap();
    let non_nulls = RowMaskView::try_new(129, &[0, u64::MAX, 1]).unwrap();
    // SAFETY: the prepared rows are NULL and hold initialized placeholders.
    let dense =
        unsafe { DenseInt32Column::try_new(&dense_values, Some(non_nulls), Some(prepared)) }
            .unwrap();
    // SAFETY: every prepared flag and Datum is initialized. Other storage may
    // be uninitialized and must not be read, even when selected by mistake.
    let datum =
        unsafe { DatumInt32Column::try_new(&datum_values, &isnull, Some(prepared)) }.unwrap();
    let later_error = RowMaskView::try_new(129, &[u64::MAX, 1, 1]).unwrap();
    let first_error = RowMaskView::try_new(129, &[0, 1, 1]).unwrap();
    for (rows, prior) in [(later_error, 64), (first_error, 0)] {
        assert_reader_error(&dense, &rows, prior);
        assert_reader_error(&datum, &rows, prior);
    }
    for row in [64, 128] {
        assert!(dense.get(row).is_err());
        assert!(datum.get(row).is_err());
    }
}

#[test]
fn byte_masks_and_sliced_values_preserve_row_numbering() {
    let offset = 5;
    let nrows = 70;
    let mut values = vec![MaybeUninit::uninit(); nrows + offset];
    let mut datums = vec![MaybeUninit::uninit(); nrows + offset];
    let mut isnull = vec![MaybeUninit::uninit(); nrows + offset];
    let mut prepared_bytes = [0xff; 10];
    let mut non_null_bytes = [0xff; 10];
    let mut selected_bytes = [0xff; 10];
    let mut expected = Vec::new();
    for row in 0..nrows {
        let physical = offset + row;
        let bit = 1 << (physical % 8);
        if row % 3 == 0 {
            prepared_bytes[physical / 8] &= !bit;
            selected_bytes[physical / 8] &= !bit;
            continue;
        }
        let is_null = row % 5 == 0;
        isnull[physical].write(is_null);
        if is_null {
            non_null_bytes[physical / 8] &= !bit;
        }
        // Prepared rows are initialized whether NULL or not.
        values[physical].write(if is_null { i32::MIN } else { row as i32 });
        datums[physical].write(if is_null { u64::MAX } else { row as u64 });
        expected.push((row, (!is_null).then_some(row as i32)));
    }
    let prepared = RowMaskView::try_from_bytes(nrows, &prepared_bytes, offset).unwrap();
    let non_nulls = RowMaskView::try_from_bytes(nrows, &non_null_bytes, offset).unwrap();
    let selected = RowMaskView::try_from_bytes(nrows, &selected_bytes, offset).unwrap();
    // SAFETY: initialized precisely the prepared rows; gaps are unprepared.
    let dense =
        unsafe { DenseInt32Column::try_new(&values[offset..], Some(non_nulls), Some(prepared)) }
            .unwrap();
    // SAFETY: initialized the flags and Datums of every prepared row.
    let datum =
        unsafe { DatumInt32Column::try_new(&datums[offset..], &isnull[offset..], Some(prepared)) }
            .unwrap();
    assert_eq!(collect(&dense, &selected), expected);
    assert_eq!(collect(&datum, &selected), expected);
    assert_fold_stops(&dense, &selected, &expected);
    assert_fold_stops(&datum, &selected, &expected);
}

#[test]
fn optional_masks_and_low_datum_bits() {
    let values = [i32::MIN, -1, 0, 1, i32::MAX];
    let dense_values = values.map(MaybeUninit::new);
    // Both sign-extended and zero-extended encodings yield the same int4.
    for datums in [
        values.map(|value| value as u64),
        values.map(|value| u64::from(value as u32)),
    ] {
        let datum_values = datums.map(MaybeUninit::new);
        let isnull = [MaybeUninit::new(false); 5];
        // SAFETY: all values are initialized and non-NULL.
        let dense = unsafe { DenseInt32Column::try_new(&dense_values, None, None) }.unwrap();
        // SAFETY: all flags are false and all int4 encodings initialized.
        let datum = unsafe { DatumInt32Column::try_new(&datum_values, &isnull, None) }.unwrap();
        let selected = RowMaskView::try_new(5, &[31]).unwrap();
        assert_eq!(collect(&dense, &selected), collect(&datum, &selected));
        let expected = collect(&dense, &selected);
        assert_fold_stops(&dense, &selected, &expected);
        assert_fold_stops(&datum, &selected, &expected);
        let bytes = RowMaskView::try_from_bytes(5, &[0b1010_0101], 2).unwrap();
        let expected = [(0, Some(i32::MIN)), (3, Some(1))];
        assert_eq!(collect(&dense, &bytes), expected);
        assert_fold_stops(&dense, &bytes, &expected);
        for (row, &value) in values.iter().enumerate() {
            assert_eq!(dense.get(row).unwrap(), Some(value));
            assert_eq!(datum.get(row).unwrap(), Some(value));
        }
        for (word, bits) in [(1, 0), (usize::MAX, 1), (0, 1 << 5)] {
            assert!(dense.word_values(word, bits).is_err());
            assert!(datum.word_values(word, bits).is_err());
        }
        let wrong = RowMaskView::try_new(1, &[1]).unwrap();
        assert!(
            dense
                .try_fold_selected(&wrong, (), |(), _, _| Ok(()))
                .is_err()
        );
        assert!(
            datum
                .try_fold_selected(&wrong, (), |(), _, _| Ok(()))
                .is_err()
        );
    }
}

#[test]
fn constructors_validate_dimensions_without_reading_storage() {
    let dense_values = [MaybeUninit::uninit(); 2];
    let datum_values = [MaybeUninit::uninit(); 2];
    let isnull = [MaybeUninit::uninit(); 2];
    let empty = RowMaskView::try_new(0, &[]).unwrap();
    let one = RowMaskView::try_new(1, &[0]).unwrap();
    let none_ready = RowMaskView::try_new(2, &[0]).unwrap();
    for wrong in [empty, one] {
        // SAFETY: no prepared rows; errors must be detected without reads.
        assert!(
            unsafe { DenseInt32Column::try_new(&dense_values, Some(wrong), Some(none_ready)) }
                .is_err()
        );
        // SAFETY: no bits are set in the supplied (incorrectly sized) mask.
        assert!(unsafe { DenseInt32Column::try_new(&dense_values, None, Some(wrong)) }.is_err());
        // SAFETY: no prepared rows need initialized flags or values.
        assert!(unsafe { DatumInt32Column::try_new(&datum_values, &isnull, Some(wrong)) }.is_err());
    }
    // SAFETY: no rows are prepared; the mismatched flag count is rejected.
    assert!(
        unsafe { DatumInt32Column::try_new(&datum_values, &isnull[..1], Some(none_ready)) }
            .is_err()
    );
    // SAFETY: empty buffers have no initialization requirements.
    let dense = unsafe { DenseInt32Column::try_new(&[], None, None) }.unwrap();
    // SAFETY: empty buffers have no initialization requirements.
    let datum = unsafe { DatumInt32Column::try_new(&[], &[], None) }.unwrap();
    assert!(collect(&dense, &empty).is_empty());
    assert!(collect(&datum, &empty).is_empty());
}

fn assert_fold_stops_at_row<C: ColumnReader<Value = i32>>(column: &C, rows: &RowMaskView<'_>) {
    let mut visited = 0;
    let stopped = column.try_fold_selected(rows, (), |(), row, _| {
        visited += 1;
        anyhow::ensure!(row != 63, "consumer stopped");
        Ok(())
    });
    assert!(stopped.is_err());
    assert_eq!(visited, 64);
    let count = column
        .try_fold_selected(rows, 0, |count, _, _| Ok(count + 1))
        .unwrap();
    assert_eq!(count, 130);
}

#[test]
fn bulk_fold_stops_at_a_consumer_error() {
    let values = [MaybeUninit::new(42); 130];
    let datums = [MaybeUninit::new(42_u64); 130];
    let nulls = [MaybeUninit::new(false); 130];
    let rows = RowMaskView::try_new(130, &[u64::MAX, u64::MAX, 3]).unwrap();
    // SAFETY: all rows are initialized and non-NULL.
    let dense = unsafe { DenseInt32Column::try_new(&values, None, None) }.unwrap();
    // SAFETY: all flags and Datums are initialized and non-NULL.
    let datum = unsafe { DatumInt32Column::try_new(&datums, &nulls, None) }.unwrap();
    assert_fold_stops_at_row(&dense, &rows);
    assert_fold_stops_at_row(&datum, &rows);
}

#[test]
fn non_nullable_prepared_column_rejects_a_whole_bad_word() {
    let mut values = [MaybeUninit::uninit(); 129];
    for value in &mut values[..65] {
        value.write(42);
    }
    let prepared = RowMaskView::try_new(129, &[u64::MAX, 1, 0]).unwrap();
    // SAFETY: every prepared row has an initialized non-NULL value; gaps do not.
    let dense = unsafe { DenseInt32Column::try_new(&values, None, Some(prepared)) }.unwrap();
    let good = RowMaskView::try_new(129, &[1, 1, 0]).unwrap();
    let expected = [(0, Some(42)), (64, Some(42))];
    assert_eq!(collect(&dense, &good), expected);
    assert_fold_stops(&dense, &good, &expected);
    let bad = RowMaskView::try_new(129, &[1, 3, 0]).unwrap();
    assert_reader_error(&dense, &bad, 1);
    assert!(dense.word_values(1, 3).is_err());
    assert!(dense.get(65).is_err());
}

#[test]
fn nullable_word_modes_preserve_rows_and_stop_on_consumer_error() {
    let nrows = 193;
    let non_null_words = [0, u64::MAX, 0xaaaa_aaaa_aaaa_aaaa, 0];
    let selected_words = [u64::MAX, u64::MAX, u64::MAX, 1];
    let mut values = vec![MaybeUninit::uninit(); nrows];
    let mut datums = vec![MaybeUninit::uninit(); nrows];
    let mut nulls = vec![MaybeUninit::new(true); nrows];
    let mut expected = Vec::new();
    let mut non_null_bytes = vec![0xff; (nrows + 7).div_ceil(8)];
    for row in 0..nrows {
        let non_null = non_null_words[row / 64] & (1 << (row % 64)) != 0;
        // Every row is prepared, so NULL rows hold placeholders.
        values[row].write(if non_null { row as i32 } else { -7 });
        datums[row].write(if non_null { row as u64 } else { 7 });
        if non_null {
            nulls[row].write(false);
        } else {
            let physical = row + 7;
            non_null_bytes[physical / 8] &= !(1 << (physical % 8));
        }
        expected.push((row, non_null.then_some(row as i32)));
    }
    let rows = RowMaskView::try_new(nrows, &selected_words).unwrap();
    // SAFETY: every flag and every Datum is initialized.
    let datum = unsafe { DatumInt32Column::try_new(&datums, &nulls, None) }.unwrap();
    for non_nulls in [
        RowMaskView::try_new(nrows, &non_null_words).unwrap(),
        RowMaskView::try_from_bytes(nrows, &non_null_bytes, 7).unwrap(),
    ] {
        // SAFETY: every row is initialized; the mask says which are NULL.
        let dense = unsafe { DenseInt32Column::try_new(&values, Some(non_nulls), None) }.unwrap();
        assert_fold_stops(&dense, &rows, &expected);
        assert_fold_stops(&datum, &rows, &expected);
    }
}
