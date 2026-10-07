//! The marks of a RIGHT or FULL join: a bit per record of the inner
//! table, set for every record a published pair matched, and the walk over
//! the records without one, which go out with NULL outer columns after the
//! outer side. The marks are the caller's memory, a run of words per
//! chunk ([`Marks`]): bit `i` of word `w` of chunk `c` stands for the
//! chunk's record `64 * w + i`. Both need only the chunks, so the size of
//! a record comes from the caller; the entry points check it against the
//! index when the table has one.

use anyhow::{Context, Result, ensure};
use tessera_core::{RowMaskView, ones};

use super::Chunks;
use super::exclusive::Cursor;
use super::header::{CHUNK_HEADER, KEY_SLOT, MAX_CHUNK_LEN, RECORD_HEADER, placement, reference};

/// The words of marks of a chunk of `chunk_len` bytes: a bit for every
/// record of `record_size` bytes after the chunk's header.
///
/// # Errors
///
/// A size that no record of a table has.
pub fn mark_words(chunk_len: usize, record_size: usize) -> Result<usize> {
    check_record_size(record_size)?;
    Ok((chunk_len.saturating_sub(CHUNK_HEADER) / record_size).div_ceil(64))
}

/// A record of a table: a multiple of 8, a header and a key at least, and
/// no more than a chunk holds after its used mark. A size between the
/// records would name places where no record starts.
fn check_record_size(size: usize) -> Result<()> {
    ensure!(
        size.is_multiple_of(8)
            && (RECORD_HEADER + KEY_SLOT..=MAX_CHUNK_LEN - CHUNK_HEADER).contains(&size),
        "records of {size} bytes are not the records of a table"
    );
    Ok(())
}

/// Division of an offset within a chunk by a record's size as a multiply
/// and a shift: exact for offsets and sizes below 2^21, and a chunk is at
/// most MAX_CHUNK_LEN, 2^20 bytes. A udiv a row was most of a mark's time.
#[derive(Clone, Copy, Debug)]
struct PerRecord {
    size: usize,
    magic: u64,
}

/// The shift of [`PerRecord`]: `ceil(2^42 / size)` errs by less than a
/// size in 2^42, under one in 2^21 of a quotient.
const SHIFT: u32 = 42;

impl PerRecord {
    fn new(size: usize) -> Result<Self> {
        ensure!(
            size > 0 && size < 2 * MAX_CHUNK_LEN,
            "records of {size} bytes in chunks of at most {MAX_CHUNK_LEN}"
        );
        Ok(Self {
            size,
            magic: (1_u64 << SHIFT).div_ceil(size as u64),
        })
    }

    /// The record an offset within a chunk lies in, and whether it starts
    /// it.
    fn record(self, offset: usize) -> (usize, bool) {
        let record = ((offset as u64 * self.magic) >> SHIFT) as usize;
        (record, record * self.size == offset)
    }

    /// The record an offset of a caller starts, below 2^21 (any 32-bit
    /// offset would overflow the multiply): `None` off a boundary or past.
    fn starting(self, offset: usize) -> Option<usize> {
        if offset >> 21 != 0 {
            return None;
        }
        let (record, starts) = self.record(offset);
        starts.then_some(record)
    }
}

/// The marks of a table's records, a run of words per chunk, each of at
/// least [`mark_words`] of the chunk's length.
pub trait Marks {
    /// Set bit `bit` of word `word` of chunk `chunk`'s marks.
    ///
    /// # Safety
    ///
    /// `chunk` is a chunk of the table, and `word` is below [`mark_words`]
    /// of its length.
    unsafe fn set(&self, chunk: usize, word: usize, bit: u64);

    /// Word `word` of chunk `chunk`'s marks.
    ///
    /// # Safety
    ///
    /// As for [`Marks::set`].
    unsafe fn word(&self, chunk: usize, word: usize) -> u64;
}

impl Chunks<'_> {
    /// The records of a chunk by its used mark, its first word: the bytes
    /// its records take, its header included, which must lie within the
    /// chunk on a record's boundary.
    fn records(&self, chunk: usize, per_record: PerRecord) -> Result<usize> {
        let len = self.lens[chunk];
        if !self.fits(chunk) {
            return Err(super::bad_chunk(chunk, len));
        }
        // SAFETY: the chunk is aligned to 8 and holds at least its 8-byte
        // header, just checked; nothing appends to it during the walk.
        let used = unsafe { self.bases[chunk].cast::<u64>().read() };
        usize::try_from(used)
            .ok()
            .filter(|&used| used <= len)
            .and_then(|used| used.checked_sub(CHUNK_HEADER))
            .and_then(|bytes| per_record.starting(bytes))
            .with_context(|| format!("table chunk {chunk} has a used mark of {used} bytes"))
    }
}

/// Set the mark of the record at `refs[row]` for each row of `rows`: a
/// reference must lie within its chunk, of `lens[chunk]` bytes, on a
/// record's boundary. The marks
/// need no more of the chunks than their lengths, so a call checks no
/// chunk but those its references name.
pub fn mark(
    lens: &[usize],
    record_size: usize,
    refs: &[u32],
    rows: &RowMaskView<'_>,
    marks: &impl Marks,
) -> Result<()> {
    let nrows = rows.nrows();
    check_record_size(record_size)?;
    let per_record = PerRecord::new(record_size)?;
    ensure!(
        refs.len() == nrows,
        "the references and the mask of the batch have different row counts"
    );
    // The references a mark names, a record's; the mark set.
    let set = |reference: u32| -> Result<()> {
        let (chunk, byte) = placement(reference);
        let len = lens.get(chunk).copied().unwrap_or(0);
        ensure!(
            byte >= CHUNK_HEADER && byte + record_size <= len,
            "table reference {reference:#x} names no record"
        );
        let (record, starts) = per_record.record(byte - CHUNK_HEADER);
        ensure!(starts, "table reference {reference:#x} names no record");
        // SAFETY: the chunk is the table's and the record within its
        // length, so its word is below the chunk's marks.
        unsafe { marks.set(chunk, record / 64, 1 << (record % 64)) };
        Ok(())
    };
    // A full word's 64 references at once, which its bits index unchecked.
    let (blocks, rest) = refs.as_chunks::<64>();
    for (index, block) in blocks.iter().enumerate() {
        for bit in ones(rows.word_at(index)) {
            set(block[bit])?;
        }
    }
    if !rest.is_empty() {
        for bit in ones(rows.word_at(blocks.len())) {
            set(*rest.get(bit).context("a row mask's bit past its rows")?)?;
        }
    }
    Ok(())
}

/// Visit the records without a mark from `cursor` on, chunk by chunk in
/// the order they were appended, as many as `out` holds: their references
/// fill `out`, the count is returned and the cursor names the first record
/// not given; 0 means the walk is over. Without marks every record is one
/// without a pair. A word of marks covers 64 records at once. A cursor
/// must name a record's place, in a chunk at most up to its used mark.
pub fn scan_unmarked(
    chunks: &Chunks<'_>,
    record_size: usize,
    cursor: &mut Cursor,
    marks: Option<&impl Marks>,
    out: &mut [u32],
) -> Result<usize> {
    check_record_size(record_size)?;
    let per_record = PerRecord::new(record_size)?;
    let first_chunk = cursor.chunk();
    let first = cursor
        .byte()
        .checked_sub(CHUNK_HEADER)
        .and_then(|offset| per_record.starting(offset))
        .filter(|_| first_chunk <= chunks.len())
        .with_context(|| format!("table cursor {:#x} lies outside the records", cursor.raw()))?;
    // Past the used mark the walk would skip the rest of the chunk.
    if first_chunk < chunks.len() {
        ensure!(
            first <= chunks.records(first_chunk, per_record)?,
            "table cursor {:#x} lies past its chunk's records",
            cursor.raw()
        );
    }
    // Every loop is bounded: by the chunks, their words and a word's bits.
    let mut count = 0;
    for chunk in first_chunk..chunks.len() {
        let end = chunks.records(chunk, per_record)?;
        let start = if chunk == first_chunk { first } else { 0 };
        for word in start / 64..end.div_ceil(64) {
            // The records of this word from `start` on, below the end.
            let low = start.saturating_sub(word * 64);
            let high = (end - word * 64).min(64);
            // SAFETY: the chunk is the table's, and the word's records lie
            // below its used mark, within its length.
            let taken = marks.map_or(0, |marks| unsafe { marks.word(chunk, word) });
            let mut free = !taken >> low << low;
            if high < 64 {
                free &= (1 << high) - 1;
            }
            for bit in ones(free) {
                let record = word * 64 + bit;
                if count == out.len() {
                    *cursor = Cursor::at(chunk, CHUNK_HEADER + record * record_size);
                    return Ok(count);
                }
                out[count] = reference(chunk, CHUNK_HEADER + record * record_size);
                count += 1;
            }
        }
    }
    *cursor = Cursor::at(chunks.len(), CHUNK_HEADER);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// The multiply and the shift divide as `/` does wherever a
        /// chunk's offsets and a record's size lie.
        #[test]
        fn a_record_is_found_as_division_finds_it(
            size in 1_usize..2 * MAX_CHUNK_LEN,
            offset in 0_usize..2 * MAX_CHUNK_LEN,
        ) {
            let (record, starts) = PerRecord::new(size).unwrap().record(offset);
            prop_assert_eq!(record, offset / size);
            prop_assert_eq!(starts, offset % size == 0);
        }
    }

    #[test]
    fn a_chunk_has_a_word_of_marks_per_64_records() {
        assert_eq!(mark_words(CHUNK_HEADER, 24).unwrap(), 0);
        assert_eq!(mark_words(CHUNK_HEADER + 64 * 24, 24).unwrap(), 1);
        assert_eq!(mark_words(CHUNK_HEADER + 65 * 24, 24).unwrap(), 2);
        assert_eq!(mark_words(CHUNK_HEADER + 65 * 24 + 23, 24).unwrap(), 2);
        // A size no record has: too small, between the records, too large.
        for size in [0, 16, 28, MAX_CHUNK_LEN] {
            assert!(mark_words(MAX_CHUNK_LEN, size).is_err(), "{size}");
        }
        assert!(mark_words(MAX_CHUNK_LEN, MAX_CHUNK_LEN - CHUNK_HEADER).is_ok());
    }

    /// No marks: a walk over every record.
    struct Unmarked;

    impl Marks for Unmarked {
        unsafe fn set(&self, _: usize, _: usize, _: u64) {}

        unsafe fn word(&self, _: usize, _: usize) -> u64 {
            0
        }
    }

    /// A walk checks a chunk when it starts on it: a chunk of a length
    /// that is not a multiple of 8, or longer than 1 MiB, is refused though
    /// its used mark reads as an empty chunk's, and a chunk that can be one
    /// is walked.
    #[test]
    fn a_walk_refuses_a_chunk_that_cannot_be_one() {
        let mut words = vec![0_u64; (MAX_CHUNK_LEN + 8) / 8];
        words[0] = CHUNK_HEADER as u64;
        let bases = [words.as_mut_ptr().cast::<u8>()];
        for (len, fits) in [
            (CHUNK_HEADER + 4 * 24 + 4, false),
            (MAX_CHUNK_LEN + 8, false),
            (MAX_CHUNK_LEN, true),
        ] {
            let lens = [len];
            // SAFETY: the base is the vector's start, valid for every length.
            let chunks = unsafe { Chunks::new(&bases, &lens) }.unwrap();
            let walked = scan_unmarked(
                &chunks,
                24,
                &mut Cursor::start(),
                None::<&Unmarked>,
                &mut [0; 4],
            );
            if fits {
                assert_eq!(walked.unwrap(), 0, "an empty chunk of {len} bytes");
            } else {
                assert!(
                    walked.is_err_and(|error| error.to_string().contains("not aligned to 8")),
                    "a chunk of {len} bytes"
                );
            }
        }
    }

    /// A walk takes a chunk's used mark only within the chunk, on a record's
    /// boundary, and a cursor only on a record's boundary below 2^21.
    #[test]
    fn a_walk_refuses_a_used_mark_or_a_cursor_off_the_records() {
        const SIZE: usize = 24;
        let len = CHUNK_HEADER + 4 * SIZE;
        let walk = |used: u64, cursor: Cursor| -> Result<usize> {
            let mut words = vec![0_u64; len / 8];
            words[0] = used;
            let bases = [words.as_mut_ptr().cast::<u8>()];
            let lens = [len];
            // SAFETY: the chunk is the vector's, used only here.
            let chunks = unsafe { Chunks::new(&bases, &lens) }?;
            let mut cursor = cursor;
            let mut out = [0; 8];
            scan_unmarked(&chunks, SIZE, &mut cursor, None::<&Unmarked>, &mut out)
        };
        let at = |byte: usize| Cursor::at(0, byte);
        assert_eq!(
            walk(len as u64, at(CHUNK_HEADER)).unwrap(),
            4,
            "a full chunk"
        );
        assert_eq!(
            walk((len - SIZE) as u64, at(CHUNK_HEADER + SIZE)).unwrap(),
            2,
            "from the second"
        );
        // A cursor past the used mark would skip the chunk's rest.
        let used = (CHUNK_HEADER + 2 * SIZE) as u64;
        assert_eq!(walk(used, at(CHUNK_HEADER + 2 * SIZE)).unwrap(), 0);
        assert!(
            walk(used, at(CHUNK_HEADER + 3 * SIZE))
                .is_err_and(|error| error.to_string().contains("past its chunk's records")),
            "a cursor past the used mark"
        );
        for used in [0, 4, len as u64 + 8, (CHUNK_HEADER + 4) as u64] {
            assert!(
                walk(used, at(CHUNK_HEADER)).is_err(),
                "a used mark of {used}"
            );
        }
        for byte in [0, CHUNK_HEADER + 4, CHUNK_HEADER + (1 << 21), 1 << 31] {
            assert!(walk(len as u64, at(byte)).is_err(), "a cursor at {byte}");
        }
        assert!(
            walk(len as u64, Cursor::at(2, CHUNK_HEADER)).is_err(),
            "past the chunks"
        );
        assert_eq!(
            walk(len as u64, Cursor::at(1, CHUNK_HEADER)).unwrap(),
            0,
            "at the end"
        );
    }

    #[test]
    fn the_largest_offsets_divide_exactly() {
        for size in [
            1,
            3,
            7,
            24,
            40,
            56,
            104,
            1000,
            MAX_CHUNK_LEN - 8,
            2 * MAX_CHUNK_LEN - 1,
        ] {
            let per_record = PerRecord::new(size).unwrap();
            for offset in [0, size - 1, size, 2 * MAX_CHUNK_LEN - 1] {
                assert_eq!(
                    per_record.record(offset).0,
                    offset / size,
                    "{offset} / {size}"
                );
            }
        }
        assert!(PerRecord::new(0).is_err());
        assert!(PerRecord::new(2 * MAX_CHUNK_LEN).is_err());
    }
}
