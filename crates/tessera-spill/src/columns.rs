//! Chunks of rows by column, for rows that are only kept and read back.
//!
//! A join's outer rows that wait for their partition are never linked or
//! probed: they are written, read once and probed then. They are kept as
//! a chunk of columns instead of the table's records: a header, then a
//! lane per column of `capacity` words each, the first the rows' NULL
//! bits (bit `w` for word `w`), then a lane per stored word (a by-value
//! Datum, or the reference of a by-reference value; 0 for a NULL). The
//! rows fill the lanes from the first place on, and a lane is read back as
//! an array of Datums, with no gathering.
//!
//! On disk a chunk's lanes are stored for its rows only, each by frame of
//! reference: the lane's least value as a signed word and every value's
//! difference from it in 1, 2, 4 or 8 bytes, the fewest that hold them
//! all, or none when every value is the same. The chunk read back has a
//! capacity of its row count.
//!
//! The header, in native byte order as every spilled block: the row count
//! and the capacity (a `u32` each), the stored words (a `u32`) and a
//! magic `u32`.

use anyhow::{Result, ensure};

/// Bytes of a chunk's header; the lanes follow.
pub const HEADER: usize = 16;

/// The most rows a chunk holds: a row's place fits the 17 bits a
/// reference to it gives it.
pub const MAX_ROWS: usize = (1 << 17) - 1;

/// The most stored words: a word of NULL bits holds one bit per word.
pub const MAX_WORDS: usize = 64;

/// Bytes a packed chunk may take past its lanes' bytes: a count word and
/// a descriptor of 16 bytes per lane.
pub const PACK_SLACK: usize = 8 + 16 * (1 + MAX_WORDS);

const ROWS_AT: usize = 0;
const CAPACITY_AT: usize = 4;
const WORDS_AT: usize = 8;
const MAGIC_AT: usize = 12;
const MAGIC: u32 = u32::from_le_bytes(*b"COLS");

fn get_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

/// The shape of a chunk: its rows, its capacity and its stored words.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shape {
    pub rows: usize,
    pub capacity: usize,
    pub words: usize,
}

impl Shape {
    /// Where lane `lane` starts: lane 0 is the NULL bits, lane `1 + w`
    /// stored word `w`.
    pub fn lane_at(&self, lane: usize) -> usize {
        HEADER + 8 * self.capacity * lane
    }
}

/// Bytes of a chunk of `capacity` rows of `words` stored words.
pub fn size(capacity: usize, words: usize) -> usize {
    HEADER + 8 * capacity * (1 + words)
}

/// The rows a chunk of `len` bytes holds, with `words` stored words.
pub fn capacity(len: usize, words: usize) -> usize {
    (len.saturating_sub(HEADER) / (8 * (1 + words))).min(MAX_ROWS)
}

/// Make the `len` bytes of `chunk` an empty chunk of `words` stored words;
/// its capacity is returned, 0 for a chunk of a header only.
pub fn init(chunk: &mut [u8], words: usize) -> Result<usize> {
    ensure!(
        chunk.len() >= HEADER && chunk.len().is_multiple_of(8),
        "a chunk of columns needs a header of {HEADER} bytes and a length of 8s, not {}",
        chunk.len()
    );
    ensure!(
        words <= MAX_WORDS,
        "a chunk of columns keeps up to {MAX_WORDS} words, not {words}"
    );
    let capacity = capacity(chunk.len(), words);
    put_u32(chunk, ROWS_AT, 0);
    put_u32(chunk, CAPACITY_AT, capacity as u32);
    put_u32(chunk, WORDS_AT, words as u32);
    put_u32(chunk, MAGIC_AT, MAGIC);
    Ok(capacity)
}

/// The shape of the chunk at `chunk`, checked against its length.
pub fn shape(chunk: &[u8]) -> Result<Shape> {
    ensure!(
        chunk.len() >= HEADER,
        "a chunk of columns is shorter than its header"
    );
    ensure!(
        get_u32(chunk, MAGIC_AT) == MAGIC,
        "the bytes hold no chunk of columns"
    );
    let shape = Shape {
        rows: get_u32(chunk, ROWS_AT) as usize,
        capacity: get_u32(chunk, CAPACITY_AT) as usize,
        words: get_u32(chunk, WORDS_AT) as usize,
    };
    ensure!(
        shape.words <= MAX_WORDS
            && shape.capacity <= MAX_ROWS
            && shape.rows <= shape.capacity
            && size(shape.capacity, shape.words) <= chunk.len(),
        "a chunk of columns of {} rows in {} places of {} words does not fit its {} bytes",
        shape.rows,
        shape.capacity,
        shape.words,
        chunk.len()
    );
    Ok(shape)
}

/// Set the row count of a chunk whose shape was read.
pub fn set_rows(chunk: &mut [u8], rows: usize) {
    put_u32(chunk, ROWS_AT, rows as u32);
}

/// Bytes a packed chunk of `rows` rows of `words` stored words may take.
pub fn pack_bound(rows: usize, words: usize) -> usize {
    8 + (1 + words) * (16 + (8 * rows).next_multiple_of(8))
}

fn lane(chunk: &[u8], shape: &Shape, lane: usize) -> impl Iterator<Item = u64> {
    let at = shape.lane_at(lane);
    chunk[at..at + 8 * shape.rows]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_ne_bytes(*word))
}

/// Store the differences of the values from `base` in `W` bytes each.
fn put<const W: usize>(values: impl Iterator<Item = u64>, base: u64, out: &mut [u8]) {
    for (value, place) in values.zip(out.as_chunks_mut::<W>().0) {
        place.copy_from_slice(&value.wrapping_sub(base).to_ne_bytes()[..W]);
    }
}

/// Load values stored in `W` bytes each as differences from `base`.
fn get<const W: usize>(packed: &[u8], base: u64, out: &mut [u8]) {
    for (place, word) in packed
        .as_chunks::<W>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<8>().0)
    {
        let mut bytes = [0_u8; 8];
        bytes[..W].copy_from_slice(place);
        *word = u64::from_ne_bytes(bytes).wrapping_add(base).to_ne_bytes();
    }
}

/// Pack the chunk at `chunk` into `out`, which needs [`pack_bound`] bytes
/// for its rows: the packed length and the length of the chunk read back
/// (its lanes for its rows only) are returned.
pub fn pack(chunk: &[u8], out: &mut [u8]) -> Result<(usize, usize)> {
    let shape = shape(chunk)?;
    let lanes = 1 + shape.words;
    ensure!(
        out.len() >= pack_bound(shape.rows, shape.words),
        "a chunk of columns of {} rows packs into up to {} bytes, not {}",
        shape.rows,
        pack_bound(shape.rows, shape.words),
        out.len()
    );
    out[..4].copy_from_slice(&(shape.rows as u32).to_ne_bytes());
    out[4..8].copy_from_slice(&(shape.words as u32).to_ne_bytes());
    let mut at = 8 + 16 * lanes;
    for index in 0..lanes {
        let (least, most) = lane(chunk, &shape, index)
            .fold((i64::MAX, i64::MIN), |(least, most), value| {
                (least.min(value as i64), most.max(value as i64))
            });
        let (width, base) = if shape.rows == 0 {
            (0, 0)
        } else {
            let span = (i128::from(most) - i128::from(least)) as u128;
            let width = match span {
                0 => 0,
                1..=0xff => 1,
                0x100..=0xffff => 2,
                0x1_0000..=0xffff_ffff => 4,
                _ => 8,
            };
            (width, least as u64)
        };
        let descriptor = 8 + 16 * index;
        out[descriptor..descriptor + 8].fill(0);
        out[descriptor] = width as u8;
        out[descriptor + 8..descriptor + 16].copy_from_slice(&base.to_ne_bytes());
        let need = (width * shape.rows).next_multiple_of(8);
        let place = &mut out[at..at + need];
        let values = lane(chunk, &shape, index);
        match width {
            0 => {}
            1 => put::<1>(values, base, place),
            2 => put::<2>(values, base, place),
            4 => put::<4>(values, base, place),
            _ => put::<8>(values, base, place),
        }
        place[width * shape.rows..].fill(0);
        at += need;
    }
    Ok((at, size(shape.rows, shape.words)))
}

/// Unpack the `packed` bytes of a chunk into `out`, of the length
/// [`pack`] returned for it: a chunk of a capacity of its row count.
pub fn unpack(packed: &[u8], out: &mut [u8]) -> Result<()> {
    ensure!(packed.len() >= 8, "a packed chunk of columns has no counts");
    let rows = get_u32(packed, 0) as usize;
    let words = get_u32(packed, 4) as usize;
    ensure!(
        rows <= MAX_ROWS && words <= MAX_WORDS && out.len() == size(rows, words),
        "a packed chunk of {rows} rows of {words} words does not unpack into {} bytes",
        out.len()
    );
    let lanes = 1 + words;
    ensure!(
        packed.len() >= 8 + 16 * lanes,
        "a packed chunk of columns is shorter than its descriptors"
    );
    init(out, words)?;
    let shape = Shape {
        rows,
        capacity: rows,
        words,
    };
    set_rows(out, rows);
    let mut at = 8 + 16 * lanes;
    for index in 0..lanes {
        let descriptor = 8 + 16 * index;
        let width = packed[descriptor] as usize;
        let base = u64::from_ne_bytes(
            packed[descriptor + 8..descriptor + 16]
                .try_into()
                .expect("eight bytes"),
        );
        ensure!(
            matches!(width, 0 | 1 | 2 | 4 | 8),
            "a packed lane of width {width}"
        );
        let need = (width * rows).next_multiple_of(8);
        ensure!(
            packed.len() >= at + need,
            "a packed chunk of columns ends inside lane {index}"
        );
        let lane_at = shape.lane_at(index);
        let target = &mut out[lane_at..lane_at + 8 * rows];
        let source = &packed[at..at + need];
        match width {
            0 => {
                for word in target.as_chunks_mut::<8>().0 {
                    *word = base.to_ne_bytes();
                }
            }
            1 => get::<1>(source, base, target),
            2 => get::<2>(source, base, target),
            4 => get::<4>(source, base, target),
            _ => get::<8>(source, base, target),
        }
        at += need;
    }
    ensure!(
        at == packed.len(),
        "a packed chunk of columns has {} bytes past its lanes",
        packed.len() - at
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_of(capacity: usize, words: usize, rows: &[Vec<u64>]) -> Vec<u8> {
        let mut chunk = vec![0_u8; size(capacity, words)];
        init(&mut chunk, words).unwrap();
        let shape = shape(&chunk).unwrap();
        for (row, lanes) in rows.iter().enumerate() {
            for (lane, value) in lanes.iter().enumerate() {
                let at = shape.lane_at(lane) + 8 * row;
                chunk[at..at + 8].copy_from_slice(&value.to_ne_bytes());
            }
        }
        set_rows(&mut chunk, rows.len());
        chunk
    }

    fn round_trip(capacity: usize, words: usize, rows: &[Vec<u64>]) -> (usize, Vec<u8>) {
        let chunk = chunk_of(capacity, words, rows);
        let mut out = vec![0_u8; pack_bound(rows.len(), words)];
        let (packed, len) = pack(&chunk, &mut out).unwrap();
        assert!(packed.is_multiple_of(8));
        let mut back = vec![0_u8; len];
        unpack(&out[..packed], &mut back).unwrap();
        let shape = shape(&back).unwrap();
        assert_eq!(
            shape,
            Shape {
                rows: rows.len(),
                capacity: rows.len(),
                words
            }
        );
        for (row, lanes) in rows.iter().enumerate() {
            for (lane, &value) in lanes.iter().enumerate() {
                let at = shape.lane_at(lane) + 8 * row;
                assert_eq!(
                    u64::from_ne_bytes(back[at..at + 8].try_into().unwrap()),
                    value,
                    "row {row} lane {lane}"
                );
            }
        }
        (packed, back)
    }

    #[test]
    fn lanes_pack_at_the_width_of_their_span_and_read_back() {
        let rows: Vec<Vec<u64>> = (0..1000_u64)
            .map(|row| {
                vec![
                    0,
                    7,
                    row % 200,
                    (row * 1000) as i64 as u64,
                    (-(row as i64) - 5) as u64,
                    row << 40,
                ]
            })
            .collect();
        let (packed, _) = round_trip(1200, 5, &rows);
        // Nothing, nothing, a byte, four bytes, two bytes, eight bytes.
        assert_eq!(
            packed,
            8 + 16 * 6 + 1000 + 4000 + 2000 + 8000,
            "packed length"
        );
    }

    #[test]
    fn empty_and_full_chunks_round_trip() {
        round_trip(10, 2, &[]);
        let rows: Vec<Vec<u64>> = (0..10)
            .map(|row| vec![row & 3, u64::MAX - row, row])
            .collect();
        round_trip(10, 2, &rows);
        round_trip(10, 0, &(0..10).map(|row| vec![row]).collect::<Vec<_>>());
    }

    #[test]
    fn a_damaged_or_foreign_chunk_is_refused() {
        let mut chunk = chunk_of(4, 1, &[vec![0, 1]]);
        let mut out = vec![0_u8; pack_bound(4, 1)];
        chunk[MAGIC_AT] ^= 1;
        assert!(pack(&chunk, &mut out).is_err());
        let chunk = chunk_of(4, 1, &[vec![0, 1], vec![0, 300]]);
        let (packed, len) = pack(&chunk, &mut out).unwrap();
        let mut back = vec![0_u8; len];
        assert!(unpack(&out[..packed - 8], &mut back).is_err());
        assert!(unpack(&out[..packed], &mut back[..len - 8]).is_err());
        let mut bad = out[..packed].to_vec();
        bad[8] = 3;
        assert!(unpack(&bad, &mut back).is_err());
        assert!(init(&mut [0_u8; 8], 1).is_err());
        assert_eq!(init(&mut [0_u8; HEADER], 3).unwrap(), 0);
    }
}
