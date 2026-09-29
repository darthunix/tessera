//! Rows appended by partition to chunks of columns
//! ([`tessera_spill::columns`]): a join's outer rows that wait for their
//! partition, kept without the table's records. Each row goes to the
//! current chunk of its hash's partition, as
//! [`crate::table::append_partitioned_columns_to`] appends records, in row order
//! as long as the chunk has room: a row whose chunk is full stays pending
//! while the rows after it go on. A row's place is referred to as the
//! table refers to a record, by the chunk's number above
//! [`PLACE_BITS`] bits of its place in the chunk.

use anyhow::{Result, ensure};
use tessera_core::RowMask;
use tessera_spill::columns;

use crate::table::{MAX_PARTITIONS, PayloadColumns};

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

/// Append the rows of `pending` to the chunks of their partitions: the
/// partition of a hash is `(hash >> shift) & (partitions - 1)`, and
/// partition `p` appends to chunk `partition_chunks[p]`. A row takes its
/// words from `columns`, one per stored word of the chunk, and its NULL
/// bits; appended rows leave `pending`, get their references in `offsets`
/// and count in `rows` at their partition. The count appended is returned.
#[allow(clippy::too_many_arguments)]
pub fn append_partitioned<C: ColumnChunks + ?Sized>(
    chunks: &mut C,
    partition_chunks: &[u32],
    shift: u32,
    hashes: &[u32],
    columns: &PayloadColumns<'_>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    rows: &mut [u64],
) -> Result<usize> {
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
        let selected = pending.as_view().word(index).unwrap();
        let mut left = selected;
        let mut done = 0;
        while left != 0 {
            let bit = left.trailing_zeros() as usize;
            left &= left - 1;
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

#[cfg(test)]
mod tests {
    use super::*;

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
            &[0, 1, 2, 3],
            2,
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
            .filter(|row| pending.as_view().word(row / 64).unwrap() >> (row % 64) & 1 == 1)
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
                &[0],
                0,
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
                &[0, 0, 0],
                0,
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
