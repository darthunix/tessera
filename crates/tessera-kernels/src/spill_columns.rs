//! Rows appended by partition to chunks of columns
//! ([`tessera_spill::columns`]): a join's outer rows that wait for their
//! partition, kept without the table's records. Each row goes to the
//! current chunk of its hash's partition, as
//! [`crate::table::append_partitioned_columns_to`] appends records, in row order
//! as long as the chunk has room: a row whose chunk is full stays pending
//! while the rows after it go on. A row's place is referred to as the
//! table refers to a record, by the chunk's number above
//! [`PLACE_BITS`] bits of its place in the chunk.

use anyhow::{Result, bail, ensure};
use tessera_core::{RowMask, ones};
use tessera_spill::columns;

use crate::table::{MAX_PARTITIONS, Partitions, PayloadColumns};

/// Bits of a row's place in a reference; the chunk's number is above them.
pub const PLACE_BITS: u32 = 17;

/// The chunks rows are appended to, by number: each lent to the caller
/// alone while it writes a row.
pub trait ColumnChunks {
    /// The bytes of chunk `index`.
    fn chunk(&mut self, index: usize) -> Result<&mut [u8]>;
}

/// Chunks each its own vector, by number.
impl ColumnChunks for [Vec<u8>] {
    fn chunk(&mut self, index: usize) -> Result<&mut [u8]> {
        let count = self.len();
        self.get_mut(index)
            .map(Vec::as_mut_slice)
            .ok_or_else(|| anyhow::anyhow!("chunk {index} of {count} chunks of columns"))
    }
}

/// Append the rows of `pending` to the chunks of their partitions, as
/// [`Partitions`] places a hash: partition `p` appends to the chunk
/// numbered `partitions.chunks[p]`. A row takes its words from `columns`,
/// one per stored word of the chunk, and its NULL bits; appended rows
/// leave `pending`, get their references in `offsets` and count in `rows`
/// at their partition. The count appended is returned.
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
        "{count} partitions are not a power of two up to {MAX_PARTITIONS}"
    );
    let bits = count.trailing_zeros();
    ensure!(
        shift < 32 && shift + bits <= 32,
        "partition bits {shift} to {} lie past the 32 bits of a hash",
        shift + bits
    );
    ensure!(
        hashes.len() == nrows && offsets.len() == nrows && columns.nrows() == nrows,
        "the hashes, offsets, columns and mask of the rows have different row counts"
    );
    ensure!(
        rows.len() == count,
        "{} row counts for {count} partitions",
        rows.len()
    );
    ensure!(
        partition_chunks
            .iter()
            .all(|&chunk| chunk < 1 << (32 - PLACE_BITS)),
        "a chunk number past {} bits",
        32 - PLACE_BITS
    );
    let mask = (count - 1) as u32;
    let width = columns.len();
    ensure!(
        width <= 64,
        "a partitioned chunk of columns takes rows of up to 64 words, not {width}"
    );
    let mut appended = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word_at(index);
        let left = selected;
        let mut done = 0;
        for bit in ones(left) {
            let row = index * 64 + bit;
            let partition = ((hashes[row] >> shift) & mask) as usize;
            let number = partition_chunks[partition];
            let chunk = chunks.chunk(number as usize)?;
            let shape = columns::shape(chunk)?;
            ensure!(
                shape.words == width,
                "a chunk of {} words takes rows of {width}",
                shape.words
            );
            if shape.rows == shape.capacity {
                continue;
            }
            let place = shape.rows;
            let mut nulls = 0_u64;
            for column in 0..width {
                let (value, null) = columns.get(column, row);
                nulls |= u64::from(null) << column;
                let at = shape.lane_at(1 + column) + 8 * place;
                chunk[at..at + 8].copy_from_slice(&value.to_ne_bytes());
            }
            let at = shape.lane_at(0) + 8 * place;
            chunk[at..at + 8].copy_from_slice(&nulls.to_ne_bytes());
            columns::set_rows(chunk, place + 1);
            offsets[row] = (number << PLACE_BITS) | place as u32;
            rows[partition] += 1;
            done |= 1 << bit;
            appended += 1;
        }
        if done != 0 {
            pending.intersect_word(index, !done)?;
        }
    }
    Ok(appended)
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

    /// Row `row`'s value of column `column`.
    ///
    /// # Errors
    ///
    /// A value whose bytes cannot be told.
    fn value(&self, column: usize, row: usize) -> Result<Value<'_>>;
}

/// What [`append`] did: the rows it appended, and the bytes of values the
/// next row takes, 0 once every row went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Appended {
    pub rows: usize,
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
/// A chunk of fewer words than the columns, `values` longer than a
/// reference names a byte of, `*used` past `values` or not aligned to 8,
/// or a value [`RowValues::value`] cannot tell, before any row of the word
/// it is in is written.
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
        "a chunk of {} words takes no row of {ncolumns} columns",
        shape.words
    );
    ensure!(
        values.len() as u64 <= 1 << columns::VALUE_BYTE_BITS,
        "a chunk of values of {} bytes is longer than a reference names",
        values.len()
    );
    ensure!(
        *used <= values.len() && used.is_multiple_of(8),
        "values used to byte {} of {}",
        *used,
        values.len()
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
            let selected = rows.as_view().word_at(index);
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
                    let word = match source.value(column, row)? {
                        Value::Null => {
                            let mut null = get(chunk, nulls + 8 * at);
                            null |= 1 << (column % 64);
                            put(chunk, nulls + 8 * at, null);
                            0
                        }
                        Value::Word(word) => word,
                        Value::Bytes(_) => bail!("a by-value column {column} gave bytes"),
                    };
                    put(chunk, lane + 8 * at, word);
                }
            }
            appended += done.count_ones() as usize;
            rows.intersect_word(index, !done)?;
        }
        columns::set_rows(chunk, place + appended);
        return Ok(Appended {
            rows: appended,
            need: 0,
        });
    }
    'words: for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word_at(index);
        let mut done = 0_u64;
        for bit in ones(selected) {
            let row = index * 64 + bit;
            let mut bytes = 0;
            for column in 0..ncolumns {
                if let Value::Bytes(value) = source.value(column, row)? {
                    bytes += value.len().next_multiple_of(8);
                }
            }
            if place == shape.capacity || *used + bytes > values.len() {
                need = bytes;
                rows.intersect_word(index, !done)?;
                break 'words;
            }
            for lane in 0..null_lanes {
                put(chunk, shape.lane_at(lane) + 8 * place, 0);
            }
            for column in 0..ncolumns {
                let word = match source.value(column, row)? {
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
        rows.intersect_word(index, !done)?;
    }
    columns::set_rows(chunk, place);
    Ok(Appended {
        rows: appended,
        need,
    })
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
    use proptest::prelude::*;
    use tessera_testing::property;

    use super::*;

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

        fn value(&self, column: usize, row: usize) -> Result<Value<'_>> {
            Ok(match &self.columns[column][row] {
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
                let mut mask = tessera_testing::words(&selected);
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
                    let left = rows.as_view().word_at(row / 64) >> (row % 64) & 1 == 1;
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
            .filter(|row| pending.as_view().word_at(row / 64) >> (row % 64) & 1 == 1)
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
    fn mismatched_shapes_are_refused() {
        let mut chunks = vec![vec![0_u8; columns::size(4, 1)]];
        columns::init(&mut chunks[0], 1).unwrap();
        let values: [&[u64]; 0] = [];
        let nulls: [&[bool]; 0] = [];
        let payload = PayloadColumns::new(&values, &nulls, 1).unwrap();
        let mut words = vec![1_u64];
        let mut pending = RowMask::try_new(1, &mut words).unwrap();
        let mut offsets = [0_u32];
        let mut rows = [0_u64];
        assert!(
            append_partitioned(
                chunks.as_mut_slice(),
                &Partitions {
                    shift: 0,
                    chunks: &[0],
                },
                &[0],
                &payload,
                &mut pending,
                &mut offsets,
                &mut rows,
            )
            .is_err()
        );
        assert!(
            append_partitioned(
                chunks.as_mut_slice(),
                &Partitions {
                    shift: 0,
                    chunks: &[0, 0, 0],
                },
                &[0],
                &payload,
                &mut pending,
                &mut offsets,
                &mut [0; 3],
            )
            .is_err()
        );
    }
}
