//! The proofs by Kani of the checks of a packed chunk of columns, for
//! every value its bytes may hold: the counts first, for any counts and
//! lengths, then the lanes, for one shape of chunk, as the copy of the
//! lanes of any shape would make the proof too large for the solver of
//! bits that Kani is.

use super::{
    Body, CAPACITY_AT, HEADER, MAX_ROWS, MAX_WORDS, ROWS_AT, WORDS_AT, body_of, check_lanes,
    copy_lanes, counts, lanes, pack_bound, size,
};
use crate::Rule;

/// The rows and the stored words of a chunk in the proofs of its lanes:
/// two rows of one word, in two lanes, of the NULL bits and of the word.
const ROWS: usize = 2;
const WORDS: usize = 1;

/// The most bytes of a packed body of the proofs' chunk: its counts, a
/// descriptor of each lane, and eight bytes a row in each lane.
const PACKED: usize = 8 + 16 * 2 + 2 * 8 * ROWS;

/// The bytes of the proofs' chunk in memory.
const CHUNK: usize = HEADER + 8 * ROWS * 2;

/// Trust point `packed-columns`: whatever counts a body holds, however
/// long the body is and whatever the length of its chunk, counts that
/// pass are within the limits of a chunk, the rows a block's packed
/// length can name among them, fill the chunk exactly, and leave room
/// for the descriptor of every lane; nothing overflows.
#[kani::proof]
fn counts_that_pass_fill_the_chunk() {
    let rows = kani::any::<u32>() as usize;
    let words = kani::any::<u32>() as usize;
    let (packed_len, out_len): (usize, usize) = (kani::any(), kani::any());
    let Ok(body) = counts(rows, words, packed_len, out_len) else {
        return;
    };
    kani::cover!(
        body.rows > 0 && body.words > 64,
        "counts of rows of more than 64 words pass"
    );
    assert!(body.rows == rows && body.words == words);
    assert!(words <= MAX_WORDS && rows <= MAX_ROWS);
    assert!(pack_bound(rows, words) <= u32::MAX as usize);
    assert!(out_len == size(rows, words));
    assert!(body.lanes == lanes(words) && 8 + 16 * body.lanes <= packed_len);
}

/// Trust point `packed-columns`: whatever descriptors and lanes a body of
/// two rows of one word holds, and however long it is up to the longest
/// body of that shape, lanes that pass
/// lie inside the body, each after the one before, and end at its last
/// byte; the copy then makes the chunk's header of its rows, capacity and
/// words, over any bytes the chunk held, and nothing panics or overflows.
/// That the lanes' values come back as they were packed is the tests'.
#[kani::proof]
#[kani::unwind(4)]
fn lanes_that_pass_lie_inside_the_body_and_fill_the_chunk() {
    let packed: [u8; PACKED] = kani::any();
    let packed_len: usize = kani::any();
    kani::assume(packed_len <= PACKED);
    let body = Body {
        rows: ROWS,
        words: WORDS,
        lanes: 2,
    };
    kani::assume(packed_len >= 8 + 16 * body.lanes);
    let packed = &packed[..packed_len];
    if check_lanes(packed, &body).is_err() {
        return;
    }
    kani::cover!(packed_len == PACKED, "the longest body of the chunk passes");
    let mut at = 8 + 16 * body.lanes;
    for index in 0..body.lanes {
        let width = packed[8 + 16 * index] as usize;
        assert!(matches!(width, 0 | 1 | 2 | 4 | 8));
        let need = (width * ROWS).next_multiple_of(8);
        assert!(at + need <= packed_len);
        at += need;
    }
    assert!(at == packed_len);
    let mut chunk: [u8; CHUNK] = kani::any();
    assert!(copy_lanes(packed, &body, &mut chunk).is_ok());
    let word = |at: usize| {
        let mut word = [0; 4];
        word.copy_from_slice(&chunk[at..at + 4]);
        u32::from_ne_bytes(word) as usize
    };
    assert!(word(ROWS_AT) == ROWS && word(CAPACITY_AT) == ROWS && word(WORDS_AT) == WORDS);
    assert!(word(12) == u32::from_le_bytes(*b"COLS") as usize);
}

/// Trust point `packed-columns`: a body too short for its counts is
/// refused before a count is read, and one long enough has its counts
/// read without a panic, whatever its bytes.
#[kani::proof]
fn a_body_too_short_for_its_counts_is_refused() {
    let packed: [u8; 8] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= packed.len());
    let result = body_of(&packed[..len], kani::any());
    if len < 8 {
        assert!(result.is_err_and(|error| error.rule() == Rule::ColumnsCounts));
    }
}
