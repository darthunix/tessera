//! Deterministic tests of the benchmark machinery, not performance measurements.
//!
//! Check the unchanged input matrices, scalar models and bounded mask preparation.
//! Counter readings are not needed: block bookkeeping is checked with a fake reader.

#[path = "../benches/support/filtering.rs"]
#[allow(dead_code)]
mod filtering;
#[path = "../benches/support/reading.rs"]
#[allow(dead_code)]
mod reading;
#[path = "../benches/support/mod.rs"]
mod support;

use anyhow::Result;
use filtering::blocks as filter_blocks;
use support::{fixture, reference};
use tessera_core::ColumnReader;

#[test]
fn initialized_views_borrow_the_original_values_without_copying() {
    fn check<T: Copy + PartialEq + std::fmt::Debug>(values: &[T]) {
        let view = fixture::as_uninit(values);
        assert_eq!(view.as_ptr().cast::<T>(), values.as_ptr());
        assert_eq!(view.len(), values.len());
        for (slot, value) in view.iter().zip(values) {
            // SAFETY: as_uninit borrows these initialized values without mutation.
            assert_eq!(unsafe { slot.assume_init() }, *value);
        }
    }
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let case = fixture::Fixture::from_values(
            (0..nrows).map(|row| row - 50).collect(),
            "all",
            "mixed",
            Some(7),
            true,
        );
        check(&case.values);
        check(&case.datums);
        check(&case.nulls);
        case.dense_column().unwrap();
        case.datum_column().unwrap();
    }
}

fn assert_sums<C: ColumnReader<Value = i32>>(
    input: &reading::Input<'_, C>,
    direct: impl Fn(&reading::Input<'_, C>) -> Result<i64>,
    expected: i64,
) {
    assert_eq!(direct(input).unwrap(), expected);
    assert_eq!(reading::fold_sum(input).unwrap(), expected);
    assert_eq!(reading::word_sum(input).unwrap(), expected);
}

#[test]
fn reference_matches_scalar_model_and_bitmap_views() {
    let cases = reading::cases();
    assert_eq!(cases.len() * 2, 36);
    for case in cases {
        let rows = case.selected.reference();
        let prepared = case.prepared.as_ref().map(fixture::Bitmap::reference);
        let non_nulls = case.non_nulls.as_ref().map(fixture::Bitmap::reference);
        assert_eq!(
            reference::dense(&case.values, rows, prepared, non_nulls).unwrap(),
            case.expected
        );
        assert_eq!(
            reference::datum(&case.datums, &case.nulls, rows, prepared).unwrap(),
            case.expected
        );
        for index in 0..rows.nrows.div_ceil(64) {
            assert_eq!(rows.word(index), case.selected.view().word(index).unwrap());
        }
        let dense = case.dense_column().unwrap();
        let datum = case.datum_column().unwrap();
        assert_sums(
            &reading::Input::new(&dense, &case),
            reading::dense_reference,
            case.expected,
        );
        assert_sums(
            &reading::Input::new(&datum, &case),
            reading::datum_reference,
            case.expected,
        );
    }
}

#[test]
fn reader_cases_are_fixed() {
    let cases = reading::cases();
    let names: Vec<_> = cases.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "words/1024/all/nulls-none/ready",
            "words/1024/half/nulls-none/ready",
            "words/1024/sparse/nulls-none/ready",
            "words/1024/empty/nulls-none/ready",
            "words/1024/all/nulls-mixed/ready",
            "words/1024/half/nulls-mixed/ready",
            "words/1024/sparse/nulls-mixed/ready",
            "words/1024/empty/nulls-mixed/ready",
            "words/0/all/nulls-mixed/ready",
            "words/1/all/nulls-mixed/ready",
            "words/63/all/nulls-mixed/ready",
            "words/64/all/nulls-mixed/ready",
            "words/65/all/nulls-mixed/ready",
            "words/1024/all/nulls-all/ready",
            "words/1024/all/nulls-mixed/partial",
            "bytes-3/65/all/nulls-mixed/partial",
            "bytes-7/1024/all/nulls-mixed/partial",
            "bytes-7/1024/sparse/nulls-mixed/partial",
        ]
    );
}

#[test]
fn filter_cases_are_fixed() {
    let cases = filtering::cases();
    assert_eq!(cases.len() * 2, 48);
    let names: Vec<_> = cases.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "bytes-0/65/all/nulls-none/ready",
            "bytes-0/65/all/nulls-mixed/ready",
            "bytes-0/65/eighth/nulls-none/ready",
            "bytes-0/65/eighth/nulls-mixed/ready",
            "bytes-0/65/one-per128/nulls-none/ready",
            "bytes-0/65/one-per128/nulls-mixed/ready",
            "bytes-0/65/empty/nulls-none/ready",
            "bytes-0/65/empty/nulls-mixed/ready",
            "bytes-0/1024/all/nulls-none/ready",
            "bytes-0/1024/all/nulls-mixed/ready",
            "bytes-0/1024/eighth/nulls-none/ready",
            "bytes-0/1024/eighth/nulls-mixed/ready",
            "bytes-0/1024/one-per128/nulls-none/ready",
            "bytes-0/1024/one-per128/nulls-mixed/ready",
            "bytes-0/1024/empty/nulls-none/ready",
            "bytes-0/1024/empty/nulls-mixed/ready",
            "bytes-0/0/all/nulls-mixed/ready",
            "bytes-0/1/all/nulls-mixed/ready",
            "bytes-0/63/all/nulls-mixed/ready",
            "bytes-0/64/all/nulls-mixed/ready",
            "words/1024/all/nulls-all/ready",
            "words/1024/all/nulls-mixed/partial",
            "bytes-7/1024/all/nulls-mixed/partial",
            "bytes-7/1024/one-per128/nulls-mixed/partial",
        ]
    );
}

fn assert_filter_model<C: ColumnReader<Value = i32>>(
    column: &C,
    case: &fixture::Fixture,
    reference: impl Fn(&filtering::Input<'_, C>, &mut [u64]) -> Result<()>,
) {
    use tessera_core::RowMask;
    use tessera_kernels::int32::CompareOp;
    for op in [
        CompareOp::Eq,
        CompareOp::Ne,
        CompareOp::Lt,
        CompareOp::Le,
        CompareOp::Gt,
        CompareOp::Ge,
    ] {
        for scalar in [i32::MIN, -1, 0, 1, i32::MAX] {
            let input = filtering::Input::new(column, case, op, scalar);
            let original = case.selected.words();
            let expected = filtering::expected(case, original, op, scalar);
            let mut direct = original.to_vec();
            reference(&input, &mut direct).unwrap();
            assert_eq!(direct, expected);
            let mut actual = original.to_vec();
            let mut mask = RowMask::try_new(case.values.len(), &mut actual).unwrap();
            filtering::scalar(&input, &mut mask).unwrap();
            filtering::scalar(&input, &mut mask).unwrap();
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn filter_references_match_both_readers_and_independent_model() {
    let mut cases = filtering::cases();
    for nulls in ["none", "mixed"] {
        cases.push(fixture::Fixture::from_values(
            vec![i32::MIN, i32::MAX, -1, 0, 1],
            "all",
            nulls,
            Some(7),
            false,
        ));
    }
    for case in cases {
        let dense = case.dense_column().unwrap();
        let datum = case.datum_column().unwrap();
        assert_filter_model(&dense, &case, filtering::dense_reference);
        assert_filter_model(&datum, &case, filtering::datum_reference);
    }
}

fn assert_filter_readiness_error<C: ColumnReader<Value = i32>>(
    column: &C,
    case: &fixture::Fixture,
    reference: impl Fn(&filtering::Input<'_, C>, &mut [u64]) -> Result<()>,
) {
    use tessera_core::RowMask;
    use tessera_kernels::int32::CompareOp;
    // Row 65 is unprepared. The first word may change, but the second must not.
    // Eq(0) removes the selected ready row 0, whose value is 42.
    let input = filtering::Input::new(column, case, CompareOp::Eq, 0);
    let mut direct = [1, 3];
    assert!(reference(&input, &mut direct).is_err());
    assert_eq!(direct, [0, 3]);
    let mut actual = [1, 3];
    let mut mask = RowMask::try_new(66, &mut actual).unwrap();
    assert!(filtering::scalar(&input, &mut mask).is_err());
    assert_eq!(actual, direct);
    // A repeated error must leave both remaining words unchanged.
    assert!(reference(&input, &mut direct).is_err());
    assert!(filtering::scalar(&input, &mut RowMask::try_new(66, &mut actual).unwrap()).is_err());
    assert_eq!(actual, [0, 3]);
    assert_eq!(actual, direct);
}

#[test]
fn filter_references_match_word_local_readiness_errors() {
    for offset in [None, Some(0), Some(7)] {
        let case = fixture::Fixture::from_values(vec![42; 66], "all", "mixed", offset, true);
        let dense = case.dense_column().unwrap();
        let datum = case.datum_column().unwrap();
        assert_filter_readiness_error(&dense, &case, filtering::dense_reference);
        assert_filter_readiness_error(&datum, &case, filtering::datum_reference);
    }
}

#[test]
fn mask_blocks_restore_every_invocation_and_handle_empty_rows() {
    use tessera_core::RowMask;
    use tessera_kernels::int32::CompareOp;
    for nrows in [0, 1, 63, 64, 65, 1024] {
        let case = fixture::Fixture::from_values(vec![42; nrows], "all", "none", None, false);
        let column = tessera_core::ColumnView::try_new(&case.values, None).unwrap();
        let input = filtering::Input::new(&column, &case, CompareOp::Eq, 0);
        let mut blocks = filter_blocks::Masks::new(nrows, case.selected.words()).unwrap();
        for count in [filter_blocks::BLOCK_SIZE, 3, filter_blocks::BLOCK_SIZE, 0] {
            let mut visited = 0;
            for words in blocks.reset(count) {
                assert_eq!(words, case.selected.words());
                filtering::scalar(&input, &mut RowMask::try_new(nrows, words).unwrap()).unwrap();
                assert!(words.iter().all(|&word| word == 0));
                visited += 1;
            }
            assert_eq!(visited, count);
        }
    }
    assert!(filter_blocks::Masks::new(65, &[u64::MAX]).is_err());
    assert!(filter_blocks::Masks::new(1, &[2]).is_err());
}

#[test]
fn counted_blocks_count_calls_and_restore_inputs_across_block_boundaries() {
    use std::cell::Cell;
    use tessera_kernels::int32::CompareOp;
    use tessera_pmu::Reading;
    for nrows in [0, 65] {
        let case = fixture::Fixture::from_values(vec![42; nrows], "all", "none", None, false);
        let column = case.dense_column().unwrap();
        let input = filtering::Input::new(&column, &case, CompareOp::Eq, 0);
        let mut masks = filter_blocks::Masks::new(nrows, case.selected.words()).unwrap();
        for count in [
            0,
            1,
            filter_blocks::BLOCK_SIZE as u64,
            filter_blocks::BLOCK_SIZE as u64 + 3,
        ] {
            // A fake reader advances by one instruction per read: each block
            // contributes exactly one counted "instruction".
            let mut reads = 0;
            let mut read = || {
                reads += 1;
                Reading {
                    instructions: reads,
                    ..Reading::default()
                }
            };
            let calls = Cell::new(0);
            let reading = masks.run_reference(
                &mut read,
                &input,
                |_, words| {
                    assert_eq!(words, case.selected.words());
                    words.fill(0);
                    calls.set(calls.get() + 1);
                    Ok(())
                },
                count,
            );
            assert_eq!(calls.get(), count);
            assert_eq!(
                reading.instructions,
                count.div_ceil(filter_blocks::BLOCK_SIZE as u64)
            );
            masks.run_scalar(&mut read, &input, count);
        }
    }
}
