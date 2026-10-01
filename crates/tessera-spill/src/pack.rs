//! Packing a chunk of records for disk.
//!
//! A table's record in memory is built for linking and probing, not for
//! storage: the reference of the next record of its bucket means nothing
//! on disk, its length is the same in every record, NULL bits are mostly
//! zero, and an `int4` key or value takes a slot of 8 bytes. A chunk of
//! records is a used mark and then records of one length; packing looks
//! at them as lanes of 4 bytes, the same lane of every record together,
//! and stores each lane at the width its values need in this chunk:
//! nothing when all are zero, one word when all are equal, or 1, 2 or 4
//! bytes each. Unpacking gives back the chunk byte for byte, except that
//! the lane of the next-record references is zero, as a chunk not yet
//! linked has it.
//!
//! The packed body: the record count and the record length (a `u32`
//! each), a code byte per lane, padded to 4, then each lane's values one
//! after another, the whole padded to 8. Native byte order, as the
//! header's.

use anyhow::Result;

use crate::Damaged;
use crate::damaged::intact;

/// Bytes of the used mark that opens a chunk.
const USED_MARK: usize = 8;
/// The record header's word holding the record length in units of 8
/// bytes, and the lane of the next-record reference.
const LEN_AT: usize = 12;
const NEXT_LANE: usize = 1;
/// The shortest record: a header of 16 bytes and one key slot.
const MIN_RECORD: usize = 24;

const ZERO: u8 = 0;
const CONSTANT: u8 = 1;
const BYTE: u8 = 2;
const HALF: u8 = 3;
const WORD: u8 = 4;

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

/// The record length of a chunk that packing can take: a used mark that
/// covers the whole chunk and at least one record, and a record length, a
/// multiple of 8 of at least [`MIN_RECORD`] bytes, that divides the rest
/// (whether every record has it is checked with the lanes).
fn record_len(chunk: &[u8]) -> Option<usize> {
    if chunk.len() < USED_MARK + MIN_RECORD || !chunk.len().is_multiple_of(8) {
        return None;
    }
    let used = u64::from_ne_bytes(chunk[..USED_MARK].try_into().expect("eight bytes"));
    if used != chunk.len() as u64 {
        return None;
    }
    let len = (word(chunk, USED_MARK + LEN_AT) as usize).checked_mul(8)?;
    (len >= MIN_RECORD && (chunk.len() - USED_MARK).is_multiple_of(len)).then_some(len)
}

/// Store lane `lane` of every record in `W` bytes each.
fn put<const W: usize>(records: &[u8], len: usize, lane: usize, out: &mut [u8]) {
    let at = lane * 4;
    for (record, place) in records.chunks_exact(len).zip(out.as_chunks_mut::<W>().0) {
        let value = word(record, at).to_ne_bytes();
        place.copy_from_slice(&value[..W]);
    }
}

/// Load lane `lane` of every record from `W` bytes each.
fn get<const W: usize>(packed: &[u8], len: usize, lane: usize, records: &mut [u8]) {
    let at = lane * 4;
    for (record, place) in records.chunks_exact_mut(len).zip(packed.as_chunks::<W>().0) {
        let mut value = [0_u8; 4];
        value[..W].copy_from_slice(place);
        record[at..at + 4].copy_from_slice(&value);
    }
}

/// Pack a chunk of records into `out`: the packed length, or `None` when
/// the chunk is not one packing takes or would not get shorter, and then
/// `out` holds nothing of use. `out` needs at most `chunk.len()` bytes.
pub fn pack(chunk: &[u8], out: &mut [u8]) -> Option<usize> {
    let len = record_len(chunk)?;
    let records = &chunk[USED_MARK..];
    let count = records.len() / len;
    let lanes = len / 4;
    let codes_end = 8 + lanes.next_multiple_of(4);
    if out.len() < codes_end || codes_end >= chunk.len() {
        return None;
    }
    // One pass over the records in order: every lane's bits, and whether
    // it differs from the first record's.
    let first: Vec<u32> = (0..lanes).map(|lane| word(records, lane * 4)).collect();
    let mut bits = vec![0_u32; lanes];
    let mut differ = vec![0_u32; lanes];
    for record in records.chunks_exact(len) {
        for ((value, (bits, differ)), first) in record
            .as_chunks::<4>()
            .0
            .iter()
            .zip(bits.iter_mut().zip(differ.iter_mut()))
            .zip(&first)
        {
            let value = u32::from_ne_bytes(*value);
            *bits |= value;
            *differ |= value ^ first;
        }
    }
    // Records of one length only.
    if differ[LEN_AT / 4] != 0 {
        return None;
    }
    out[..4].copy_from_slice(&(count as u32).to_ne_bytes());
    out[4..8].copy_from_slice(&(len as u32).to_ne_bytes());
    out[8..codes_end].fill(0);
    let mut at = codes_end;
    for lane in 0..lanes {
        let code = match (bits[lane], differ[lane]) {
            _ if lane == NEXT_LANE => ZERO,
            (0, _) => ZERO,
            (_, 0) => CONSTANT,
            (0..=0xff, _) => BYTE,
            (0..=0xffff, _) => HALF,
            _ => WORD,
        };
        out[8 + lane] = code;
        let need = match code {
            ZERO => 0,
            CONSTANT => 4,
            BYTE => count,
            HALF => 2 * count,
            _ => 4 * count,
        };
        if at + need >= chunk.len() || at + need > out.len() {
            return None;
        }
        let place = &mut out[at..at + need];
        match code {
            ZERO => {}
            CONSTANT => place.copy_from_slice(&first[lane].to_ne_bytes()),
            BYTE => put::<1>(records, len, lane, place),
            HALF => put::<2>(records, len, lane, place),
            _ => put::<4>(records, len, lane, place),
        }
        at += need;
    }
    let end = at.next_multiple_of(8);
    if end >= chunk.len() || end > out.len() {
        return None;
    }
    out[at..end].fill(0);
    Some(end)
}

/// Unpack a body [`pack`] made into `chunk`, which must be exactly the
/// chunk's length.
pub fn unpack(packed: &[u8], chunk: &mut [u8]) -> Result<()> {
    intact!(
        packed.len() >= 8,
        "a packed chunk of {} bytes has no counts",
        packed.len()
    );
    let count = word(packed, 0) as usize;
    let len = word(packed, 4) as usize;
    intact!(
        len >= MIN_RECORD
            && len.is_multiple_of(8)
            && count > 0
            && count
                .checked_mul(len)
                .and_then(|bytes| bytes.checked_add(USED_MARK))
                == Some(chunk.len()),
        "a packed chunk of {count} records of {len} bytes does not fill {} bytes",
        chunk.len()
    );
    let lanes = len / 4;
    let codes_end = 8 + lanes.next_multiple_of(4);
    intact!(
        packed.len() >= codes_end,
        "a packed chunk is shorter than its lane codes"
    );
    let used = chunk.len() as u64;
    chunk[..USED_MARK].copy_from_slice(&used.to_ne_bytes());
    let records = &mut chunk[USED_MARK..];
    let mut at = codes_end;
    for lane in 0..lanes {
        let code = packed[8 + lane];
        let need = match code {
            ZERO => 0,
            CONSTANT => 4,
            BYTE => count,
            HALF => 2 * count,
            WORD => 4 * count,
            other => return Err(Damaged(format!("a packed chunk has lane code {other}")).into()),
        };
        intact!(
            at + need <= packed.len(),
            "a packed chunk ends inside its lane {lane}"
        );
        let place = &packed[at..at + need];
        match code {
            ZERO | CONSTANT => {
                let value = if code == ZERO {
                    [0_u8; 4]
                } else {
                    place.try_into().expect("four bytes")
                };
                for record in records.chunks_exact_mut(len) {
                    record[lane * 4..lane * 4 + 4].copy_from_slice(&value);
                }
            }
            BYTE => get::<1>(place, len, lane, records),
            HALF => get::<2>(place, len, lane, records),
            _ => get::<4>(place, len, lane, records),
        }
        at += need;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk of `count` records of `len` bytes: a hash, a next link, NULL
    /// bits, the length, then words the closure gives per record and lane.
    fn chunk(count: usize, len: usize, word: impl Fn(usize, usize) -> u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; USED_MARK + count * len];
        let total = bytes.len() as u64;
        bytes[..8].copy_from_slice(&total.to_ne_bytes());
        for record in 0..count {
            let base = USED_MARK + record * len;
            for lane in 0..len / 4 {
                let value = match lane {
                    1 => 12345,
                    3 => (len / 8) as u32,
                    _ => word(record, lane),
                };
                bytes[base + lane * 4..base + lane * 4 + 4].copy_from_slice(&value.to_ne_bytes());
            }
        }
        bytes
    }

    fn round_trip(original: &[u8]) -> usize {
        let mut out = vec![0_u8; original.len()];
        let packed = pack(original, &mut out).expect("packs");
        let mut back = vec![0_u8; original.len()];
        unpack(&out[..packed], &mut back).unwrap();
        let mut expected = original.to_vec();
        let len = record_len(original).unwrap();
        for record in 0..(original.len() - USED_MARK) / len {
            let at = USED_MARK + record * len + 4;
            expected[at..at + 4].fill(0);
        }
        assert_eq!(back, expected);
        packed
    }

    #[test]
    fn a_join_record_of_int4_packs_to_twelve_bytes() {
        // Hash, key low half, NULL-bits word zero, an int4 value.
        let original = chunk(1000, 40, |record, lane| match lane {
            0 => (record as u32).wrapping_mul(2_654_435_761),
            4 => 1_000_000 + record as u32,
            8 => 7 * record as u32 + 100_000,
            _ => 0,
        });
        let packed = round_trip(&original);
        assert!(packed <= 8 + 12 + 12 * 1000 + 8, "{packed} bytes");
    }

    #[test]
    fn widths_follow_the_values_of_each_lane() {
        let original = chunk(300, 32, |record, lane| match lane {
            0 => record as u32 * 7919,
            2 => 0,
            4 => (record % 200) as u32,
            5 => (record * 100) as u32,
            6 => 42,
            _ => 0,
        });
        round_trip(&original);
    }

    #[test]
    fn what_is_no_chunk_of_records_stays_as_it_is() {
        let mut out = vec![0_u8; 4096];
        assert_eq!(pack(&[0_u8; 12], &mut out), None);
        assert_eq!(pack(&[7_u8; 64], &mut out), None, "a used mark that lies");
        let mut mixed = chunk(4, 32, |_, _| 1);
        mixed[USED_MARK + 32 + LEN_AT..USED_MARK + 32 + LEN_AT + 4]
            .copy_from_slice(&5_u32.to_ne_bytes());
        assert_eq!(pack(&mixed, &mut out), None, "records of two lengths");
        let dense = chunk(2, 24, |record, lane| {
            (record * 1_000_003 + lane) as u32 | 0x8000_0000
        });
        assert_eq!(pack(&dense, &mut out), None, "nothing to gain");
    }

    #[test]
    fn damaged_packed_bodies_are_refused() {
        let original = chunk(10, 24, |record, _| record as u32 + 1);
        let mut out = vec![0_u8; original.len()];
        let packed = pack(&original, &mut out).unwrap();
        let mut back = vec![0_u8; original.len()];
        // Bytes read back that fail their checks are damaged data.
        let damaged = |result: Result<()>| {
            result
                .expect_err("accepted")
                .downcast_ref::<crate::Damaged>()
                .is_some()
        };
        assert!(damaged(unpack(&out[..4], &mut back)));
        assert!(damaged(unpack(
            &out[..packed],
            &mut back[..original.len() - 8]
        )));
        let mut bad = out[..packed].to_vec();
        bad[8] = 9;
        assert!(damaged(unpack(&bad, &mut back)));
        assert!(damaged(unpack(&out[..12], &mut back)));
    }
}
