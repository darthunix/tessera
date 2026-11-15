//! The proofs by Kani of the checks of a packed body of records, for
//! every value its bytes may hold: the counts first, for any counts and
//! lengths, then the lanes, for one shape of records, as the copy of the
//! lanes of any shape would make the proof too large for the solver of
//! bits that Kani is.

use super::{Body, body_of, check_lanes, copy_lanes, counts, stored};
use crate::Rule;

/// The records of a body in the proofs of its lanes: two of the shortest
/// records, of a header and one key.
const COUNT: usize = 2;
const LEN: usize = 24;

/// The most bytes of a packed body of the proofs' records: its counts,
/// its codes, the lane of the records' length, which is constant, and
/// four bytes a record in each of the four lanes that hold data, padded
/// to 8; the lane of the next records is empty.
const PACKED: usize = (8 + 8 + 4 + 4 * 4 * COUNT).next_multiple_of(8);

/// The bytes of a chunk of the proofs' records, with its used mark.
const CHUNK: usize = 8 + COUNT * LEN;

/// Trust point `packed-records`: whatever counts a body holds, however
/// long the body is and whatever the length of its chunk, counts that
/// pass fill the chunk with records of a record's length, and the codes
/// of all their lanes lie in the body; nothing overflows.
#[kani::proof]
fn counts_that_pass_fill_the_chunk() {
    let count = kani::any::<u32>() as usize;
    let len = kani::any::<u32>() as usize;
    let (packed_len, chunk_len): (usize, usize) = (kani::any(), kani::any());
    let Ok(body) = counts(count, len, packed_len, chunk_len) else {
        return;
    };
    kani::cover!(body.len > 4096, "counts of records past 4 KiB pass");
    assert!(body.count == count && body.len == len);
    assert!(body.len >= 24 && body.len.is_multiple_of(8) && body.count > 0);
    assert!(body.count.checked_mul(body.len) == Some(chunk_len - 8));
    assert!(body.lanes == body.len / 4);
    assert!(body.codes_end == 8 + (body.len / 4).next_multiple_of(4));
    assert!(body.codes_end <= packed_len);
}

/// Trust point `packed-records`: whatever codes and lanes a body of two
/// records of 24 bytes holds, and however long it is, lanes that pass
/// lie inside the body, each after the one before, and end at its last
/// bytes, padded to 8; the copy then fills the chunk, its used mark the
/// chunk's length and every record of the body's length with no next
/// record, and nothing panics or overflows, whatever bytes the chunk held
/// before. That the data lanes come back as they were packed is the
/// tests'.
#[kani::proof]
#[kani::unwind(7)]
fn lanes_that_pass_lie_inside_the_body_and_fill_the_chunk() {
    let packed: [u8; PACKED] = kani::any();
    let packed_len: usize = kani::any();
    kani::assume(packed_len <= PACKED);
    let body = Body {
        count: COUNT,
        len: LEN,
        lanes: LEN / 4,
        codes_end: 16,
    };
    kani::assume(packed_len >= body.codes_end);
    let packed = &packed[..packed_len];
    if check_lanes(packed, &body).is_err() {
        return;
    }
    kani::cover!(
        packed_len == PACKED,
        "the longest body of the records passes"
    );
    let mut at = body.codes_end;
    for lane in 0..body.lanes {
        let need = stored(packed[8 + lane], COUNT);
        assert!(need.is_some());
        let need = need.unwrap_or(0);
        assert!(at + need <= packed_len);
        at += need;
    }
    assert!(at.next_multiple_of(8) == packed_len);
    let mut chunk: [u8; CHUNK] = kani::any();
    copy_lanes(packed, &body, &mut chunk);
    let word = |bytes: &[u8], at: usize| {
        let mut word = [0; 4];
        word.copy_from_slice(&bytes[at..at + 4]);
        u32::from_ne_bytes(word)
    };
    let mut used = [0; 8];
    used.copy_from_slice(&chunk[..8]);
    assert!(u64::from_ne_bytes(used) == CHUNK as u64);
    for record in chunk[8..].chunks_exact(LEN) {
        assert!(word(record, relhash::RECORD_NEXT_OFFSET) == 0);
        assert!(word(record, relhash::RECORD_SIZE_OFFSET) as usize == LEN / 8);
    }
}

/// Trust point `packed-records`: a body too short for its counts is
/// refused before a count is read, and one long enough has its counts
/// read without a panic, whatever its bytes.
#[kani::proof]
fn a_body_too_short_for_its_counts_is_refused() {
    let packed: [u8; 8] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= packed.len());
    let result = body_of(&packed[..len], kani::any());
    if len < 8 {
        assert!(result.is_err_and(|error| error.rule() == Rule::PackedCounts));
    }
}
