//! The writer and the reader of chunks of columns ([`crate::columns`]).
//!
//! [`append`] writes a source's rows into one chunk, their values by
//! reference copied into the chunk's own chunk of values, and stops where
//! the chunk or the values are full. The two are written as a [`PAIR`] of
//! blocks, the values first, or the chunk alone when it has no values. A
//! reader checks the pair's blocks ([`check_pair`]) and the chunk read
//! back with its values ([`check`]) once, then reads windows of its rows
//! ([`window`]) unchecked.
//!
//! [`append_partitioned`] appends rows by partition: a join's outer rows
//! that wait for their partition, kept without the table's records. Each
//! row goes to the current chunk of its hash's partition, as
//! [`ChunkWriter::append_partitioned_columns`](relhash::ChunkWriter::append_partitioned_columns)
//! appends records, in row order as long as the chunk has room: a row
//! whose chunk is full stays pending while the rows after it go on. A
//! row's place is referred to as the table refers to a record, by the
//! chunk's number above [`PLACE_BITS`] bits of its place in the chunk.

use std::ops::Range;

use relhash::{MAX_PARTITIONS, Partitions, PayloadColumns, RowMask, ones};

use crate::columns::{self, Shape};
use crate::error::ensure;
use crate::{BlockHeader, BlockKind, Error, Result, Rule};

/// Bits of a row's place in a reference; the chunk's number is above them.
pub const PLACE_BITS: u32 = 17;

/// The chunks rows are appended to, by number: each lent to the caller
/// alone while it writes a row.
pub trait ColumnChunks {
    /// The bytes of chunk `index`, or `None` when the host cannot give
    /// it: the append then fails with [`Rule::ChunkRead`], and a host that
    /// must say why keeps its own error.
    fn chunk(&mut self, index: usize) -> Option<&mut [u8]>;
}

/// Chunks each its own vector, by number.
impl ColumnChunks for [Vec<u8>] {
    fn chunk(&mut self, index: usize) -> Option<&mut [u8]> {
        self.get_mut(index).map(Vec::as_mut_slice)
    }
}

/// A mask's refusal of a word, which the loops' bounds never meet.
fn masked(result: core::result::Result<(), relhash::Error>) -> Result<()> {
    result.map_err(|_| Error::wrong_call(Rule::Mask))
}

/// Append the rows of `pending` to the chunks of their partitions, as
/// [`Partitions`] places a hash: partition `p` appends to the chunk
/// numbered `partitions.chunks[p]`. A row takes its words from `columns`,
/// one per stored word of the chunk, and its NULL bits; appended rows
/// leave `pending`, get their references in `offsets` and count in `rows`
/// at their partition. The count appended is returned.
///
/// # Errors
///
/// Partitions, bits, row counts or chunk numbers out of their bounds,
/// before anything is written; or a chunk the host cannot give, or that
/// is no chunk of columns of the rows' words, which may come after rows
/// of the same word of `pending` were appended while `pending` still
/// holds them: the chunks, `offsets`, `rows` and `pending` are then to be
/// dropped.
pub fn append_partitioned<C: ColumnChunks + ?Sized>(
    chunks: &mut C,
    partitions: &Partitions<'_>,
    hashes: &[u32],
    columns: &PayloadColumns<'_>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    rows: &mut [u64],
) -> Result<usize> {
    let Partitions {
        shift,
        chunks: partition_chunks,
    } = *partitions;
    let nrows = pending.as_view().nrows();
    let count = partition_chunks.len();
    ensure!(
        count.is_power_of_two() && count <= MAX_PARTITIONS,
        Error::wrong_call(Rule::Partitions)
            .with(count)
            .and(MAX_PARTITIONS)
    );
    let bits = count.trailing_zeros();
    ensure!(
        shift < 32 && shift + bits <= 32,
        Error::wrong_call(Rule::PartitionBits)
            .with(shift)
            .and(shift.saturating_add(bits))
    );
    ensure!(
        hashes.len() == nrows && offsets.len() == nrows && columns.nrows() == nrows,
        Error::wrong_call(Rule::RowCounts)
    );
    ensure!(
        rows.len() == count,
        Error::wrong_call(Rule::PartitionRows)
            .with(rows.len())
            .and(count)
    );
    ensure!(
        partition_chunks
            .iter()
            .all(|&chunk| chunk < 1 << (32 - PLACE_BITS)),
        Error::wrong_call(Rule::ChunkNumber).with(32 - PLACE_BITS)
    );
    let mask = (count - 1) as u32;
    let width = columns.len();
    let mut appended = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index);
        let left = selected;
        let mut done = 0;
        for bit in ones(left) {
            let row = index * 64 + bit;
            let partition = ((hashes[row] >> shift) & mask) as usize;
            let number = partition_chunks[partition];
            let chunk = chunks
                .chunk(number as usize)
                .ok_or_else(|| Error::wrong_call(Rule::ChunkRead).with(number))?;
            let shape = columns::shape(chunk)?;
            ensure!(
                shape.words == width,
                Error::wrong_call(Rule::ChunkWidth)
                    .with(shape.words)
                    .and(width)
            );
            if shape.rows == shape.capacity {
                continue;
            }
            let place = shape.rows;
            let lanes = shape.word_lane(0);
            // Up to 64 words, one lane without a loop over lanes, which
            // would cost every row some 20 instructions.
            if lanes == 1 {
                write_lane(chunk, &shape, columns, row, 0, 0..width);
            } else {
                write_lanes(chunk, &shape, columns, row);
            }
            columns::set_rows(chunk, place + 1);
            offsets[row] = (number << PLACE_BITS) | place as u32;
            rows[partition] += 1;
            done |= 1 << bit;
            appended += 1;
        }
        if done != 0 {
            masked(pending.intersect_word(index, !done))?;
        }
    }
    Ok(appended)
}

/// Row `row`'s words of `words` into their lanes at the chunk's next
/// place, and their NULL bits into lane `lane`. Always inlined: as a call
/// of its own, its loop over the words keeps bounds checks the caller's
/// would hoist.
#[inline(always)]
fn write_lane(
    chunk: &mut [u8],
    shape: &columns::Shape,
    columns: &PayloadColumns<'_>,
    row: usize,
    lane: usize,
    words: Range<usize>,
) {
    let (place, first) = (shape.rows, shape.word_lane(0));
    let mut nulls = 0_u64;
    for column in words {
        let (value, null) = columns.get(column, row);
        nulls |= u64::from(null) << (column % 64);
        let at = shape.lane_at(first + column) + 8 * place;
        chunk[at..at + 8].copy_from_slice(&value.to_ne_bytes());
    }
    let at = shape.lane_at(lane) + 8 * place;
    chunk[at..at + 8].copy_from_slice(&nulls.to_ne_bytes());
}

/// Row `row`'s words and NULL bits when they take more than one lane:
/// rare, and kept out of the loop of the rows, whose registers it would
/// take.
#[cold]
#[inline(never)]
fn write_lanes(chunk: &mut [u8], shape: &columns::Shape, columns: &PayloadColumns<'_>, row: usize) {
    for lane in 0..shape.word_lane(0) {
        let words = 64 * lane..shape.words.min(64 * lane + 64);
        write_lane(chunk, shape, columns, row, lane, words);
    }
}

/// A row's value of a column, as a chunk of columns stores it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value<'a> {
    /// SQL NULL: the word 0 and the column's NULL bit.
    Null,
    /// A by-value Datum: the word itself.
    Word(u64),
    /// A by-reference value's bytes, copied into the values: the word is
    /// their offset there.
    Bytes(&'a [u8]),
}

/// The values of a batch's rows, by column, that [`append`] writes.
pub trait RowValues {
    /// The columns, the chunk's first words.
    fn ncolumns(&self) -> usize;

    /// Whether every column is by value, so that no row has bytes to copy.
    fn by_value(&self) -> bool;

    /// Row `row`'s value of column `column`, or `None` when the source
    /// cannot tell its bytes: the append then fails with
    /// [`Rule::ValueRead`], which names the column and the row, and a
    /// source that must say why gives its own error for them.
    fn value(&self, column: usize, row: usize) -> Option<Value<'_>>;
}

/// Row `row`'s value of `column`, or the error that names them.
#[inline(always)]
fn value_of(source: &impl RowValues, column: usize, row: usize) -> Result<Value<'_>> {
    source
        .value(column, row)
        .ok_or_else(|| Error::wrong_call(Rule::ValueRead).with(column).and(row))
}

/// What [`append`] did: the rows it appended, and the bytes of values the
/// next row takes, 0 once every row went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Appended {
    /// The rows appended.
    pub rows: usize,
    /// The bytes of values the next row takes; 0 once every row went.
    pub need: usize,
}

/// Append the rows of `rows`, in their order, to the chunk of columns
/// `chunk` after its rows: column `c` of a row is the chunk's word `c`, the
/// words past the columns the caller's (a sort's keys). A by-reference
/// value is copied into `values` at `*used`, its offset aligned to 8, and
/// the word is a reference to it ([`columns::value_ref`] of chunk 0, the
/// one chunk of values of this chunk of columns). It stops when the chunk
/// is full or the next row's values would pass the end of `values`; the
/// rows appended leave `rows`.
///
/// # Errors
///
/// A chunk that is no chunk of columns or has fewer words than the
/// columns, `values` longer than a reference names a byte of, or `*used`
/// past `values` or not aligned to 8, before anything is written; or a
/// value [`RowValues::value`] cannot tell, or bytes from a source by
/// value, which may come after rows were written past the chunk's count,
/// values copied, `*used` moved and words of the mask taken: the chunk,
/// `values`, `*used` and `rows` are then to be dropped.
pub fn append(
    chunk: &mut [u8],
    source: &impl RowValues,
    rows: &mut RowMask<'_>,
    values: &mut [u8],
    used: &mut usize,
) -> Result<Appended> {
    let shape = columns::shape(chunk)?;
    let ncolumns = source.ncolumns();
    ensure!(
        ncolumns <= shape.words,
        Error::wrong_call(Rule::ChunkColumns)
            .with(shape.words)
            .and(ncolumns)
    );
    ensure!(
        values.len() as u64 <= 1 << columns::VALUE_BYTE_BITS,
        Error::wrong_call(Rule::ValuesLength).with(values.len())
    );
    ensure!(
        *used <= values.len() && used.is_multiple_of(8),
        Error::wrong_call(Rule::ValuesUsed)
            .with(values.len())
            .and(*used)
    );
    let null_lanes = columns::null_lanes(shape.words);
    let nrows = rows.as_view().nrows();
    let mut place = shape.rows;
    let mut appended = 0;
    let mut need = 0;
    if source.by_value() {
        // Column by column, a lane at a time: no row has bytes, so the
        // rows that go are the first that fit.
        let room = shape.capacity - shape.rows;
        let mut taken = 0;
        for index in 0..nrows.div_ceil(64) {
            if taken == room {
                break;
            }
            let selected = rows.as_view().word(index);
            let mut done = 0_u64;
            for bit in ones(selected) {
                if taken == room {
                    break;
                }
                let at = place + taken;
                for lane in 0..null_lanes {
                    put(chunk, shape.lane_at(lane) + 8 * at, 0);
                }
                taken += 1;
                done |= 1 << bit;
            }
            for column in 0..ncolumns {
                let lane = shape.lane_at(shape.word_lane(column));
                let nulls = shape.lane_at(column / 64);
                for (at, bit) in (place + appended..).zip(ones(done)) {
                    let row = index * 64 + bit;
                    let word = match value_of(source, column, row)? {
                        Value::Null => {
                            let mut null = get(chunk, nulls + 8 * at);
                            null |= 1 << (column % 64);
                            put(chunk, nulls + 8 * at, null);
                            0
                        }
                        Value::Word(word) => word,
                        Value::Bytes(_) => {
                            return Err(Error::wrong_call(Rule::ByValueBytes).with(column));
                        }
                    };
                    put(chunk, lane + 8 * at, word);
                }
            }
            appended += done.count_ones() as usize;
            masked(rows.intersect_word(index, !done))?;
        }
        columns::set_rows(chunk, place + appended);
        return Ok(Appended {
            rows: appended,
            need: 0,
        });
    }
    'words: for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word(index);
        let mut done = 0_u64;
        for bit in ones(selected) {
            let row = index * 64 + bit;
            let mut bytes = 0;
            for column in 0..ncolumns {
                if let Value::Bytes(value) = value_of(source, column, row)? {
                    bytes += value.len().next_multiple_of(8);
                }
            }
            if place == shape.capacity || *used + bytes > values.len() {
                need = bytes;
                masked(rows.intersect_word(index, !done))?;
                break 'words;
            }
            for lane in 0..null_lanes {
                put(chunk, shape.lane_at(lane) + 8 * place, 0);
            }
            for column in 0..ncolumns {
                let word = match value_of(source, column, row)? {
                    Value::Null => {
                        let nulls = shape.lane_at(column / 64) + 8 * place;
                        let mut null = get(chunk, nulls);
                        null |= 1 << (column % 64);
                        put(chunk, nulls, null);
                        0
                    }
                    Value::Word(word) => word,
                    Value::Bytes(value) => {
                        let at = *used;
                        values[at..at + value.len()].copy_from_slice(value);
                        *used = at + value.len().next_multiple_of(8);
                        columns::value_ref(0, at)
                    }
                };
                put(
                    chunk,
                    shape.lane_at(shape.word_lane(column)) + 8 * place,
                    word,
                );
            }
            place += 1;
            appended += 1;
            done |= 1 << bit;
        }
        masked(rows.intersect_word(index, !done))?;
    }
    columns::set_rows(chunk, place);
    Ok(Appended {
        rows: appended,
        need,
    })
}

/// The blocks a chunk of columns is written as, in this order and under
/// one number: the chunk of its values, then the chunk of its rows. A
/// reader so has the values in hand before the words that refer to them.
/// A chunk with no values is written as the chunk of its rows alone: a
/// block of values, even an empty one, would cost a read of its own.
pub const PAIR: [BlockKind; 2] = [BlockKind::Values, BlockKind::Columns];

/// Whether `header`, read back as block `at` of the [`PAIR`] numbered
/// `number`, is that block. The first block may be the chunk of rows of
/// a chunk with no values, which is then the whole pair: the caller
/// tells the two by the header's kind.
///
/// # Errors
///
/// An `at` past the pair, a wrong call; a block of another kind or
/// another number, damaged bytes.
pub fn check_pair(header: &BlockHeader, at: usize, number: u32) -> Result<()> {
    let kind = *PAIR
        .get(at)
        .ok_or_else(|| Error::wrong_call(Rule::PairBlock).with(at))?;
    ensure!(
        header.kind == kind || (at == 0 && header.kind == BlockKind::Columns),
        Error::damaged(Rule::PairKind)
            .with(header.kind as u32)
            .and(kind as u32)
    );
    ensure!(
        header.number == number,
        Error::damaged(Rule::PairNumber)
            .with(header.number)
            .and(number)
    );
    Ok(())
}

/// Where a value read back ends, which its type tells: a rule of the
/// host's, not of the format.
pub trait ValueEnds {
    /// Whether the value of column `column` that starts `rest`, the bytes
    /// of its chunk of values from its reference on, ends inside them.
    fn ends(&self, column: usize, rest: &[u8]) -> bool;
}

/// Check a chunk of columns read back with its chunk of `values`, in one
/// pass, so that its rows are read unchecked after: it holds `words`
/// words and, when given, `rows` rows; and every word of a column not
/// marked in `by_value`, which flags the chunk's first words, of a row
/// that is not NULL, refers to its one chunk of values
/// ([`columns::value_ref`] of chunk 0), to a byte of it or to its end,
/// where [`append`] places an empty value, and the value there ends inside
/// the values, as `ends` tells by the value's type.
///
/// # Errors
///
/// More columns in `by_value` than words, a wrong call; a chunk read back
/// that is no chunk of
/// columns, another count of words or of rows, a word that refers outside
/// the values, or a value that runs past them, damaged bytes.
pub fn check(
    chunk: &[u8],
    words: usize,
    rows: Option<usize>,
    by_value: &[bool],
    values: &[u8],
    ends: &impl ValueEnds,
) -> Result<Shape> {
    let values_len = values.len();
    let shape = columns::shape(chunk).map_err(|error| {
        let (a, b) = error.numbers();
        Error::damaged(error.rule()).with(a).and(b)
    })?;
    ensure!(
        by_value.len() <= words,
        Error::wrong_call(Rule::ChunkColumns)
            .with(words)
            .and(by_value.len())
    );
    ensure!(
        shape.words == words,
        Error::damaged(Rule::ReadWords).with(shape.words).and(words)
    );
    if let Some(rows) = rows {
        ensure!(
            shape.rows == rows,
            Error::damaged(Rule::ReadRows).with(shape.rows).and(rows)
        );
    }
    let byte_mask = (1_u64 << columns::VALUE_BYTE_BITS) - 1;
    for column in (0..by_value.len()).filter(|&column| !by_value[column]) {
        let nulls = shape.lane_at(column / 64);
        let lane = shape.lane_at(shape.word_lane(column));
        // The column's lanes of its rows, taken once.
        let (null_words, _) = chunk[nulls..nulls + 8 * shape.rows].as_chunks::<8>();
        let (words_of, _) = chunk[lane..lane + 8 * shape.rows].as_chunks::<8>();
        let bit = column % 64;
        for (row, (null, word)) in null_words.iter().zip(words_of).enumerate() {
            if u64::from_ne_bytes(*null) >> bit & 1 != 0 {
                continue;
            }
            let word = u64::from_ne_bytes(*word);
            let byte = (word & byte_mask) as usize;
            ensure!(
                word >> columns::VALUE_BYTE_BITS == 1 && byte <= values_len,
                Error::damaged(Rule::ValueRef).with(row).and(column)
            );
            let rest = &values[byte..];
            ensure!(
                ends.ends(column, rest),
                Error::damaged(Rule::ValueEnd).with(row).and(column)
            );
        }
    }
    Ok(shape)
}

/// The words of word `word` of a chunk of columns for its rows from
/// `first` on, as many as `out` holds, into `out`, and whether each is
/// NULL into `nulls`; a NULL's word is 0. A chunk read back is read so
/// once [`check`] passed it.
///
/// # Errors
///
/// A wrong call: a chunk that is no chunk of columns, a word past its
/// words, rows past its rows, or `out` and `nulls` of different lengths.
pub fn window(
    chunk: &[u8],
    word: usize,
    first: usize,
    out: &mut [u64],
    nulls: &mut [bool],
) -> Result<()> {
    let shape = columns::shape(chunk)?;
    let count = out.len();
    ensure!(
        word < shape.words,
        Error::wrong_call(Rule::ChunkColumns)
            .with(shape.words)
            .and(word.saturating_add(1))
    );
    ensure!(nulls.len() == count, Error::wrong_call(Rule::RowCounts));
    ensure!(
        first <= shape.rows && count <= shape.rows - first,
        Error::wrong_call(Rule::Window)
            .with(first.saturating_add(count))
            .and(shape.rows)
    );
    let null_at = shape.lane_at(word / 64) + 8 * first;
    let lane_at = shape.lane_at(shape.word_lane(word)) + 8 * first;
    let (lane, _) = chunk[lane_at..lane_at + 8 * count].as_chunks::<8>();
    let (null_lane, _) = chunk[null_at..null_at + 8 * count].as_chunks::<8>();
    let bit = word % 64;
    // A window of no NULL is its words as they lie.
    let any = null_lane
        .iter()
        .fold(0, |any, bits| any | u64::from_ne_bytes(*bits));
    if any >> bit & 1 == 0 {
        for (word, from) in out.iter_mut().zip(lane) {
            *word = u64::from_ne_bytes(*from);
        }
        nulls.fill(false);
        return Ok(());
    }
    for (((word, null), from), bits) in out.iter_mut().zip(nulls).zip(lane).zip(null_lane) {
        *null = u64::from_ne_bytes(*bits) >> bit & 1 != 0;
        // A NULL's word is 0 whatever the chunk holds: the check skips it.
        *word = if *null { 0 } else { u64::from_ne_bytes(*from) };
    }
    Ok(())
}

#[inline]
fn get(chunk: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&chunk[at..at + 8]);
    u64::from_ne_bytes(word)
}

#[inline]
fn put(chunk: &mut [u8], at: usize, word: u64) {
    chunk[at..at + 8].copy_from_slice(&word.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use std::panic::Location;

    use anyhow::{Result, ensure};
    use proptest::prelude::*;
    use proptest::test_runner::{FileFailurePersistence, TestCaseError, TestRunner};

    use super::*;
    use crate::Kind;

    /// Run `test` on the cases of `strategy`, and fail with the smallest
    /// failing case, which `spill_columns.proptest-regressions` beside
    /// this file keeps for the next runs.
    #[track_caller]
    #[allow(
        clippy::panic,
        reason = "a failed property fails its test, as an assertion does"
    )]
    fn property<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<()>) {
        let config = ProptestConfig {
            source_file: Some(Location::caller().file()),
            failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
                "proptest-regressions",
            ))),
            ..ProptestConfig::default()
        };
        let outcome = TestRunner::new(config).run(&strategy, |case| {
            test(case).map_err(|error| TestCaseError::fail(format!("{error:?}")))
        });
        if let Err(failure) = outcome {
            panic!("{failure}");
        }
    }

    /// The words of a mask of `flags`: row `r` at bit `r % 64` of word
    /// `r / 64`.
    fn mask_words(flags: &[bool]) -> Vec<u64> {
        let mut words = vec![0; flags.len().div_ceil(64)];
        for (row, &flag) in flags.iter().enumerate() {
            words[row / 64] |= u64::from(flag) << (row % 64);
        }
        words
    }

    /// A test's column of values by row.
    #[derive(Clone, Debug)]
    enum Cell {
        Null,
        Word(u64),
        Bytes(Vec<u8>),
    }

    #[derive(Clone, Debug)]
    struct Batch {
        columns: Vec<Vec<Cell>>,
        by_value: bool,
    }

    impl RowValues for Batch {
        fn ncolumns(&self) -> usize {
            self.columns.len()
        }

        fn by_value(&self) -> bool {
            self.by_value
        }

        fn value(&self, column: usize, row: usize) -> Option<Value<'_>> {
            Some(match &self.columns[column][row] {
                Cell::Null => Value::Null,
                Cell::Word(word) => Value::Word(*word),
                Cell::Bytes(bytes) => Value::Bytes(bytes),
            })
        }
    }

    /// A batch by value or with bytes, a selection, a capacity the batch
    /// often does not fit, and a flag and a number the test makes the budget
    /// of. A batch is long, up to 150 rows (three words of a mask) of a few
    /// columns, or wide, a few rows of 60 to 70 columns (the second lane of
    /// NULL bits). Batches long and wide at once took eight times the
    /// cells, 9 s of every mutant's run, and caught no mutant more
    /// (plan 9.21).
    fn batches() -> impl Strategy<Value = (Batch, Vec<bool>, usize, usize, (bool, usize))> {
        let shapes = prop_oneof![
            3 => (1..150_usize, 1..6_usize),
            1 => (1..12_usize, 60..70_usize),
        ];
        (shapes, any::<bool>()).prop_flat_map(|((nrows, ncolumns), by_value)| {
            let cell = if by_value {
                prop_oneof![1 => Just(Cell::Null), 4 => any::<u64>().prop_map(Cell::Word)].boxed()
            } else {
                prop_oneof![
                    1 => Just(Cell::Null),
                    2 => any::<u64>().prop_map(Cell::Word),
                    2 => proptest::collection::vec(any::<u8>(), 0..20).prop_map(Cell::Bytes),
                ]
                .boxed()
            };
            (
                proptest::collection::vec(proptest::collection::vec(cell, nrows), ncolumns)
                    .prop_map(move |columns| Batch { columns, by_value }),
                proptest::collection::vec(any::<bool>(), nrows),
                1..nrows + nrows / 3 + 2,
                0..3_usize,
                (any::<bool>(), 0..8 * nrows * ncolumns + 64),
            )
        })
    }

    #[test]
    fn rows_go_in_order_until_the_chunk_or_the_values_fill() {
        property(
            batches(),
            |(batch, selected, capacity, extra, (exact, room))| -> Result<()> {
                let nrows = selected.len();
                let ncolumns = batch.columns.len();
                let take = |row: usize| -> usize {
                    batch
                        .columns
                        .iter()
                        .map(|column| match &column[row] {
                            Cell::Bytes(value) => value.len().next_multiple_of(8),
                            _ => 0,
                        })
                        .sum()
                };
                // Half the cases have the budget the first of the selected
                // rows fill to the byte.
                let budget = if exact {
                    let chosen: Vec<usize> = (0..nrows).filter(|&row| selected[row]).collect();
                    let fit = &chosen[..room % (chosen.len() + 1)];
                    fit.iter().map(|&row| take(row)).sum()
                } else {
                    room
                };
                let words = ncolumns + extra;
                let mut chunk = vec![0_u8; columns::size(capacity, words)];
                columns::init(&mut chunk, words)?;
                // Half the cases append after rows already there.
                let before = if extra % 2 == 1 { capacity / 3 } else { 0 };
                columns::set_rows(&mut chunk, before);
                let mut mask = mask_words(&selected);
                let mut rows = RowMask::try_new(nrows, &mut mask)?;
                let mut values = vec![0_u8; budget];
                let mut used = 0;
                let got = append(&mut chunk, &batch, &mut rows, &mut values, &mut used)?;
                // The model: the selected rows in order while the chunk has
                // room and their bytes fit.
                let (mut place, mut bytes, mut need) = (before, 0, 0);
                let mut went = vec![false; nrows];
                for row in (0..nrows).filter(|&row| selected[row]) {
                    let take = take(row);
                    if place == capacity || bytes + take > budget {
                        need = take;
                        break;
                    }
                    for (column, cells) in batch.columns.iter().enumerate() {
                        let word = lane_word(&chunk, columns::null_lanes(words) + column, place);
                        let null = lane_word(&chunk, column / 64, place) >> (column % 64) & 1;
                        match &cells[row] {
                            Cell::Null => {
                                ensure!((word, null) == (0, 1), "row {row} column {column}")
                            }
                            Cell::Word(value) => {
                                ensure!((word, null) == (*value, 0), "row {row} column {column}")
                            }
                            Cell::Bytes(value) => {
                                // A reference to chunk 0: 1 above the byte.
                                let at = (word & u64::from(u32::MAX)) as usize;
                                ensure!(
                                    word == columns::value_ref(0, at)
                                        && word >> columns::VALUE_BYTE_BITS == 1
                                        && null == 0
                                        && at.is_multiple_of(8)
                                        && values[at..at + value.len()] == value[..],
                                    "row {row} column {column}"
                                );
                            }
                        }
                    }
                    bytes += take;
                    place += 1;
                    went[row] = true;
                }
                ensure!(
                    got == Appended {
                        rows: place - before,
                        need
                    },
                    "{got:?} for {place} rows, {need}"
                );
                ensure!(
                    used == bytes && columns::shape(&chunk)?.rows == place,
                    "{used} bytes used"
                );
                for (row, &went) in went.iter().enumerate() {
                    let left = rows.as_view().word(row / 64) >> (row % 64) & 1 == 1;
                    ensure!(left == (selected[row] && !went), "row {row} left {left}");
                }
                Ok(())
            },
        );
    }

    fn lane_word(chunk: &[u8], lane: usize, place: usize) -> u64 {
        let shape = columns::shape(chunk).unwrap();
        let at = shape.lane_at(lane) + 8 * place;
        u64::from_ne_bytes(chunk[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn rows_go_to_their_partitions_until_a_chunk_fills() {
        let nrows = 100;
        let mut chunks: Vec<Vec<u8>> = [0, 10, 100, 100]
            .iter()
            .map(|&capacity| {
                let mut chunk = vec![0_u8; columns::size(capacity, 2)];
                columns::init(&mut chunk, 2).unwrap();
                chunk
            })
            .collect();
        let first: Vec<u64> = (0..nrows as u64).map(|row| row * 3).collect();
        let second: Vec<u64> = (0..nrows as u64).map(|row| row << 40).collect();
        let first_nulls: Vec<bool> = (0..nrows).map(|row| row % 7 == 1).collect();
        let second_nulls = vec![false; nrows];
        let values = [first.as_slice(), second.as_slice()];
        let nulls = [first_nulls.as_slice(), second_nulls.as_slice()];
        let payload = PayloadColumns::new(&values, &nulls, nrows).unwrap();
        // Partition bits 2 and 3; partition 0 has chunk 0, which has no room.
        let hashes: Vec<u32> = (0..nrows as u32).map(|row| (row % 4) << 2).collect();
        let mut words = vec![u64::MAX, (1 << 36) - 1];
        let mut pending = RowMask::try_new(nrows, &mut words).unwrap();
        let mut offsets = vec![0_u32; nrows];
        let mut rows = [0_u64; 4];
        let appended = append_partitioned(
            chunks.as_mut_slice(),
            &Partitions {
                shift: 2,
                chunks: &[0, 1, 2, 3],
            },
            &hashes,
            &payload,
            &mut pending,
            &mut offsets,
            &mut rows,
        )
        .unwrap();
        assert_eq!(rows, [0, 10, 25, 25]);
        assert_eq!(appended, 60);
        let left: Vec<usize> = (0..nrows)
            .filter(|row| pending.as_view().word(row / 64) >> (row % 64) & 1 == 1)
            .collect();
        let expected: Vec<usize> = (0..nrows)
            .filter(|row| row % 4 == 0 || (row % 4 == 1 && *row >= 40))
            .collect();
        assert_eq!(left, expected);
        for row in (0..nrows).filter(|row| !expected.contains(row)) {
            let chunk = &chunks[(offsets[row] >> PLACE_BITS) as usize];
            let place = (offsets[row] & ((1 << PLACE_BITS) - 1)) as usize;
            assert_eq!((offsets[row] >> PLACE_BITS) as usize, row % 4);
            assert_eq!(place, row / 4);
            let null = first_nulls[row];
            assert_eq!(lane_word(chunk, 0, place), u64::from(null));
            assert_eq!(
                lane_word(chunk, 1, place),
                if null { 0 } else { first[row] }
            );
            assert_eq!(lane_word(chunk, 2, place), second[row]);
        }
    }

    #[test]
    fn rows_wider_than_64_words_keep_a_lane_of_null_bits_per_64() {
        // One lane to its last bit, a second lane of one word, and three
        // lanes, the last for words 128 and 129 alone.
        for width in [64, 65, 130] {
            let nrows = 5;
            let lanes = columns::null_lanes(width);
            let mut chunks = vec![vec![0_u8; columns::size(8, width)]];
            columns::init(&mut chunks[0], width).unwrap();
            let values: Vec<Vec<u64>> = (0..width as u64)
                .map(|column| (0..nrows as u64).map(|row| row * 1000 + column).collect())
                .collect();
            let nulls: Vec<Vec<bool>> = (0..width)
                .map(|column| (0..nrows).map(|row| (row + column) % 5 == 0).collect())
                .collect();
            let value_slices: Vec<&[u64]> = values.iter().map(Vec::as_slice).collect();
            let null_slices: Vec<&[bool]> = nulls.iter().map(Vec::as_slice).collect();
            let payload = PayloadColumns::new(&value_slices, &null_slices, nrows).unwrap();
            let mut words = vec![(1 << nrows) - 1];
            let mut pending = RowMask::try_new(nrows, &mut words).unwrap();
            let mut offsets = vec![0_u32; nrows];
            let mut rows = [0_u64];
            let appended = append_partitioned(
                chunks.as_mut_slice(),
                &Partitions {
                    shift: 0,
                    chunks: &[0],
                },
                &[0; 5],
                &payload,
                &mut pending,
                &mut offsets,
                &mut rows,
            )
            .unwrap();
            assert_eq!(appended, nrows);
            for place in 0..nrows {
                for lane in 0..lanes {
                    let expected = (64 * lane..width.min(64 * lane + 64))
                        .filter(|&column| nulls[column][place])
                        .fold(0_u64, |bits, column| bits | 1 << (column % 64));
                    assert_eq!(
                        lane_word(&chunks[0], lane, place),
                        expected,
                        "lane {lane} of row {place}, {width} words"
                    );
                }
                for column in 0..width {
                    let null = nulls[column][place];
                    assert_eq!(
                        lane_word(&chunks[0], lanes + column, place),
                        if null { 0 } else { values[column][place] },
                        "word {column} of row {place}, {width} words"
                    );
                }
            }
        }
    }

    #[test]
    fn mismatched_shapes_are_refused() {
        let mut chunks = vec![vec![0_u8; columns::size(4, 1)]];
        columns::init(&mut chunks[0], 1).unwrap();
        let values: [&[u64]; 0] = [];
        let nulls: [&[bool]; 0] = [];
        let payload = PayloadColumns::new(&values, &nulls, 1).unwrap();
        let mut words = vec![1_u64];
        let mut pending = RowMask::try_new(1, &mut words).unwrap();
        let mut offsets = [0_u32];
        let mut refused = |shift, numbers: &[u32], hashes: &[u32], rows: &mut [u64]| {
            append_partitioned(
                chunks.as_mut_slice(),
                &Partitions {
                    shift,
                    chunks: numbers,
                },
                hashes,
                &payload,
                &mut pending,
                &mut offsets,
                rows,
            )
            .expect_err("appended")
        };
        let wrong = |rule, a, b| (Kind::WrongCall, rule, (a, b));
        let seen = |error: Error| (error.kind(), error.rule(), error.numbers());
        // A chunk of one word for rows of none, a chunk the chunks cannot
        // give, and a chunk number past a reference's bits.
        let error = refused(0, &[0], &[0], &mut [0]);
        assert_eq!(seen(error), wrong(Rule::ChunkWidth, 1, 0));
        let error = refused(0, &[1], &[0], &mut [0]);
        assert_eq!(seen(error), wrong(Rule::ChunkRead, 1, 0));
        let error = refused(0, &[1 << 15], &[0], &mut [0]);
        assert_eq!(seen(error), wrong(Rule::ChunkNumber, 15, 0));
        // Partitions that are no power of two, bits past the hash's, and
        // counts that are not the rows' or the partitions'.
        let error = refused(0, &[0, 0, 0], &[0], &mut [0; 3]);
        assert_eq!(
            seen(error),
            wrong(Rule::Partitions, 3, MAX_PARTITIONS as u32)
        );
        let error = refused(32, &[0], &[0], &mut [0]);
        assert_eq!(seen(error), wrong(Rule::PartitionBits, 32, 32));
        let error = refused(0, &[0], &[0, 0], &mut [0]);
        assert_eq!(seen(error), wrong(Rule::RowCounts, 0, 0));
        let error = refused(0, &[0], &[0], &mut [0; 2]);
        assert_eq!(seen(error), wrong(Rule::PartitionRows, 2, 1));
    }

    /// A source of two columns: the first NULL in every row, the second
    /// giving `cell`.
    struct One {
        cell: Option<Cell>,
        by_value: bool,
    }

    impl RowValues for One {
        fn ncolumns(&self) -> usize {
            2
        }

        fn by_value(&self) -> bool {
            self.by_value
        }

        fn value(&self, column: usize, _: usize) -> Option<Value<'_>> {
            if column == 0 {
                return Some(Value::Null);
            }
            match self.cell.as_ref()? {
                Cell::Null => Some(Value::Null),
                Cell::Word(word) => Some(Value::Word(*word)),
                Cell::Bytes(bytes) => Some(Value::Bytes(bytes)),
            }
        }
    }

    /// Each check of an append names its rule, its kind and its numbers:
    /// a chunk too narrow, values used past an aligned byte, a value the
    /// source cannot give, and bytes from a source by value, at row 3.
    #[test]
    fn an_append_names_the_rule_it_refuses_by() {
        let refused = |words: usize, source: &One, used: usize| {
            let mut chunk = vec![0_u8; columns::size(4, words)];
            columns::init(&mut chunk, words).unwrap();
            let mut mask = vec![1_u64 << 3];
            let mut rows = RowMask::try_new(4, &mut mask).unwrap();
            let mut values = vec![0_u8; 64];
            let mut used = used;
            append(&mut chunk, source, &mut rows, &mut values, &mut used).expect_err("appended")
        };
        let wrong = |rule, a, b| (Kind::WrongCall, rule, (a, b));
        let seen = |error: Error| (error.kind(), error.rule(), error.numbers());
        let word = One {
            cell: Some(Cell::Word(7)),
            by_value: true,
        };
        assert_eq!(seen(refused(1, &word, 0)), wrong(Rule::ChunkColumns, 1, 2));
        let error = refused(2, &word, 4);
        assert_eq!(seen(error), wrong(Rule::ValuesUsed, 64, 4));
        assert_eq!(error.to_string(), "values used to byte 4 of 64");
        assert_eq!(seen(refused(2, &word, 72)), wrong(Rule::ValuesUsed, 64, 72));
        for by_value in [true, false] {
            let none = One {
                cell: None,
                by_value,
            };
            let error = refused(2, &none, 0);
            assert_eq!(
                seen(error),
                wrong(Rule::ValueRead, 1, 3),
                "by value {by_value}"
            );
        }
        let bytes = One {
            cell: Some(Cell::Bytes(vec![1, 2, 3])),
            by_value: true,
        };
        assert_eq!(seen(refused(2, &bytes, 0)), wrong(Rule::ByValueBytes, 1, 0));
    }

    /// A host whose values end anywhere.
    struct Whole;

    impl ValueEnds for Whole {
        fn ends(&self, _: usize, _: &[u8]) -> bool {
            true
        }
    }

    /// A batch of `nrows` rows of `ncolumns` columns, columns 1 and 65 by
    /// reference and the others by value, a NULL in every third of the
    /// first 64 rows of each, so that the windows after them have none;
    /// and which columns are by value.
    fn read_batch(nrows: usize, ncolumns: usize) -> (Batch, Vec<bool>) {
        let columns = (0..ncolumns)
            .map(|column| {
                (0..nrows)
                    .map(|row| match (row + column) % 3 {
                        0 if row < 64 => Cell::Null,
                        _ if column % 64 == 1 => Cell::Bytes(vec![row as u8; row % 13]),
                        _ => Cell::Word((row * 1000 + column) as u64),
                    })
                    .collect()
            })
            .collect();
        let batch = Batch {
            columns,
            by_value: false,
        };
        (
            batch,
            (0..ncolumns).map(|column| column % 64 != 1).collect(),
        )
    }

    /// The batch appended whole to a chunk of a word past its columns, as
    /// a sort's key, and the values it used.
    fn written(batch: &Batch, nrows: usize) -> Result<(Vec<u8>, Vec<u8>, usize)> {
        let words = batch.columns.len() + 1;
        let mut chunk = vec![0_u8; columns::size(nrows, words)];
        columns::init(&mut chunk, words)?;
        let mut mask = mask_words(&vec![true; nrows]);
        let mut rows = RowMask::try_new(nrows, &mut mask)?;
        let mut values = vec![0_u8; 16 * nrows];
        let mut used = 0;
        let got = append(&mut chunk, batch, &mut rows, &mut values, &mut used)?;
        ensure!(got.rows == nrows, "{got:?}");
        values.truncate(used);
        Ok((chunk, values, words))
    }

    /// A chunk read back, checked once, reads by windows of up to 64 rows
    /// every word, NULL and value it was written with: a long chunk of a
    /// few columns, whose windows cross words of rows and the last of
    /// which hold no NULL, and one of 70 columns, past the first lane of
    /// NULL bits, whose last rows hold none in either lane.
    #[test]
    fn a_chunk_read_back_is_checked_then_read_by_windows() -> Result<()> {
        for (nrows, ncolumns) in [(150, 3), (70, 70)] {
            let (batch, by_value) = read_batch(nrows, ncolumns);
            let (chunk, values, words) = written(&batch, nrows)?;
            let shape = check(&chunk, words, Some(nrows), &by_value, &values, &Whole)?;
            ensure!((shape.rows, shape.words) == (nrows, words));
            check(&chunk, words, None, &by_value, &values, &Whole)?;
            for (column, cells) in batch.columns.iter().enumerate() {
                for first in (0..nrows).step_by(64).chain([nrows]) {
                    let count = 64.min(nrows - first);
                    let mut out = vec![7; count];
                    let mut nulls = vec![true; count];
                    window(&chunk, column, first, &mut out, &mut nulls)?;
                    for (at, (&word, &null)) in out.iter().zip(&nulls).enumerate() {
                        let row = first + at;
                        let same = match &cells[row] {
                            Cell::Null => (word, null) == (0, true),
                            Cell::Word(value) => (word, null) == (*value, false),
                            Cell::Bytes(value) => {
                                let at = (word - columns::value_ref(0, 0)) as usize;
                                !null && values[at..at + value.len()] == value[..]
                            }
                        };
                        ensure!(same, "row {row} of column {column}: {word}, {null}");
                    }
                }
            }
        }
        Ok(())
    }

    /// A chunk read back of another count of words or of rows, or with a
    /// word of a value that refers past the values, to another chunk, or
    /// to nothing, is damaged bytes; a pair's block of another kind or
    /// number too, though the rows alone are the pair of a chunk with no
    /// values. A call past the chunk's words or rows, or past the pair, is
    /// a wrong call.
    #[test]
    fn a_chunk_read_back_of_another_shape_or_reference_is_damaged() -> Result<()> {
        let wrong = |error: Error| (error.kind(), error.rule());
        let damaged = |rule| Err((Kind::Damaged, rule));
        let nrows = 10;
        let (batch, by_value) = read_batch(nrows, 3);
        let (chunk, values, words) = written(&batch, nrows)?;
        let len = values.len();
        ensure!(
            check(&chunk, words + 1, None, &by_value, &values, &Whole)
                .map(|_| ())
                .map_err(wrong)
                == damaged(Rule::ReadWords)
        );
        ensure!(
            check(&chunk, words, Some(nrows + 1), &by_value, &values, &Whole)
                .map(|_| ())
                .map_err(wrong)
                == damaged(Rule::ReadRows)
        );
        for (row, cells) in batch.columns[1].iter().enumerate() {
            let Cell::Bytes(_) = cells else { continue };
            for word in [columns::value_ref(0, len + 8), columns::value_ref(1, 0), 0] {
                let mut stray = chunk.clone();
                let shape = columns::shape(&stray)?;
                put(
                    &mut stray,
                    shape.lane_at(shape.word_lane(1)) + 8 * row,
                    word,
                );
                ensure!(
                    check(&stray, words, Some(nrows), &by_value, &values, &Whole)
                        .map(|_| ())
                        .map_err(wrong)
                        == damaged(Rule::ValueRef),
                    "row {row}, word {word:#x}"
                );
            }
        }
        let too_many = vec![true; words + 1];
        ensure!(
            check(&chunk, words, None, &too_many, &values, &Whole).map_err(wrong)
                == Err((Kind::WrongCall, Rule::ChunkColumns))
        );
        // A value that runs past the values, by the host's rule.
        struct Short;
        impl ValueEnds for Short {
            fn ends(&self, column: usize, rest: &[u8]) -> bool {
                column != 1 || rest.len() > 64
            }
        }
        ensure!(
            check(&chunk, words, None, &by_value, &values, &Short)
                .map(|_| ())
                .map_err(wrong)
                == damaged(Rule::ValueEnd)
        );
        let (mut out, mut nulls) = (vec![0; 4], vec![false; 4]);
        for word in [words, usize::MAX] {
            ensure!(
                window(&chunk, word, 0, &mut out, &mut nulls).map_err(wrong)
                    == Err((Kind::WrongCall, Rule::ChunkColumns))
            );
        }
        ensure!(
            window(&chunk, 0, nrows - 3, &mut out, &mut nulls).map_err(wrong)
                == Err((Kind::WrongCall, Rule::Window))
        );
        ensure!(
            window(&chunk, 0, 0, &mut out, &mut nulls[..3]).map_err(wrong)
                == Err((Kind::WrongCall, Rule::RowCounts))
        );
        // A chunk read back that is no chunk of columns.
        let mut foreign = chunk.clone();
        foreign[12..16].copy_from_slice(&[0; 4]);
        ensure!(
            check(&foreign, words, None, &by_value, &values, &Whole)
                .map(|_| ())
                .map_err(wrong)
                == damaged(Rule::ColumnsMagic)
        );

        let header = |kind, number| BlockHeader {
            kind,
            number,
            partition: 0,
            level: 0,
            fingerprint: 0,
            len: 8,
            packed: 0,
        };
        check_pair(&header(BlockKind::Values, 3), 0, 3)?;
        check_pair(&header(BlockKind::Columns, 3), 1, 3)?;
        // A chunk with no values: its rows alone.
        check_pair(&header(BlockKind::Columns, 3), 0, 3)?;
        for (kind, at) in [
            (BlockKind::Records, 0),
            (BlockKind::Values, 1),
            (BlockKind::Records, 1),
        ] {
            ensure!(
                check_pair(&header(kind, 3), at, 3).map_err(wrong)
                    == Err((Kind::Damaged, Rule::PairKind)),
                "{kind:?} as block {at}"
            );
        }
        ensure!(
            check_pair(&header(BlockKind::Columns, 4), 0, 3).map_err(wrong)
                == Err((Kind::Damaged, Rule::PairNumber))
        );
        ensure!(
            check_pair(&header(BlockKind::Values, 4), 0, 3).map_err(wrong)
                == Err((Kind::Damaged, Rule::PairNumber))
        );
        ensure!(
            check_pair(&header(BlockKind::Values, 3), 2, 3).map_err(wrong)
                == Err((Kind::WrongCall, Rule::PairBlock))
        );
        Ok(())
    }

    /// An empty value, which `append` places at the end of the values, is
    /// a reference the check takes, as the last value and alone; and a
    /// NULL row's word reads 0 though the chunk holds another.
    #[test]
    fn an_empty_value_reads_back_and_a_null_reads_zero() -> Result<()> {
        let batch = Batch {
            columns: vec![vec![
                Cell::Bytes(b"abc".to_vec()),
                Cell::Null,
                Cell::Bytes(Vec::new()),
            ]],
            by_value: false,
        };
        for nrows in [3, 1] {
            let alone = Batch {
                columns: vec![batch.columns[0][3 - nrows..].to_vec()],
                by_value: false,
            };
            let (mut chunk, values, words) = written(&alone, nrows)?;
            check(&chunk, words, Some(nrows), &[false], &values, &Whole)?;
            let (mut out, mut nulls) = (vec![7; nrows], vec![false; nrows]);
            window(&chunk, 0, 0, &mut out, &mut nulls)?;
            let last = out[nrows - 1] - columns::value_ref(0, 0);
            ensure!(last == values.len() as u64, "{out:?} over {}", values.len());
            if nrows == 3 {
                let shape = columns::shape(&chunk)?;
                put(&mut chunk, shape.lane_at(shape.word_lane(0)) + 8, 77);
                check(&chunk, words, Some(nrows), &[false], &values, &Whole)?;
                window(&chunk, 0, 0, &mut out, &mut nulls)?;
                ensure!((out[1], nulls[1]) == (0, true), "{out:?}");
            }
        }
        Ok(())
    }
}
