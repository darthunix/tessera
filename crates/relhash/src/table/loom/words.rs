//! The models of words that participants change at once with an atomic
//! OR: the bits of a filter filled together and the marks of a join.

use crate::{RowMask, RowMaskView};
use ::loom::sync::Arc;
use ::loom::sync::atomic::AtomicU64;
use ::loom::thread;

use crate::table::Chunks;
use crate::table::bloom::{self, FilterOr, FilterRead};
use crate::table::header::{CHUNK_HEADER, Place};
use crate::table::marks::{self, Marks};
use crate::table::region::order;
use crate::table::writer::Cursor;

/// A word of loom atomics changed by an atomic OR or, for the tests that
/// the model notices, by a plain read and a write.
struct LoomWord {
    word: AtomicU64,
    plain: bool,
}

impl LoomWord {
    fn new(plain: bool) -> Self {
        Self {
            word: AtomicU64::new(0),
            plain,
        }
    }

    fn or(&self, mask: u64) {
        if self.plain {
            let old = self.word.load(order::RELAXED);
            self.word.store(old | mask, order::RELAXED);
        } else {
            self.word.fetch_or(mask, order::RELAXED);
        }
    }
}

/// A filter of one word, without a state word, that participants fill at
/// once, as a spilling join's filter of every inner row.
impl FilterRead for LoomWord {
    fn nwords(&self) -> usize {
        1
    }

    fn load(&self, _: usize) -> u64 {
        self.word.load(order::RELAXED)
    }
}

impl FilterOr for LoomWord {
    fn or(&self, _: usize, mask: u64) {
        LoomWord::or(self, mask);
    }
}

/// The marks of a chunk of up to 64 records, set by every participant.
impl Marks for LoomWord {
    unsafe fn set(&self, _: usize, _: usize, bit: u64) {
        self.or(bit);
    }

    unsafe fn word(&self, _: usize, _: usize) -> u64 {
        self.word.load(order::LOAD)
    }
}

/// Two hashes whose four bits lie in the one word of a filter of one word,
/// each with a bit the other lacks, so that the bits of either can be lost.
fn two_hashes() -> [u32; 2] {
    let (_, first) = bloom::place(1, 64);
    let second = (2..)
        .find(|&hash| {
            let (_, mask) = bloom::place(hash, 64);
            mask & !first != 0 && first & !mask != 0
        })
        .unwrap();
    [1, second]
}

/// Two participants add a key each to one word of a filter at once; once
/// both are done, as after the barrier that orders the additions before
/// the probes, the filter lets both keys through.
fn participants_add_to_one_word(plain: bool) {
    let hashes = two_hashes();
    ::loom::model(move || {
        let filter = Arc::new(LoomWord::new(plain));
        let threads: Vec<_> = hashes
            .iter()
            .map(|&hash| {
                let filter = filter.clone();
                thread::spawn(move || {
                    let one = [1];
                    let rows = RowMaskView::try_new(1, &one).unwrap();
                    let shift = bloom::shift_for(1).unwrap();
                    bloom::add_with(&*filter, shift, &[hash], &rows).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let words = [filter.load(0)];
        let both = [0b11];
        let rows = RowMaskView::try_new(2, &both).unwrap();
        let mut found_bits = [0];
        let mut found = RowMask::try_new(2, &mut found_bits).unwrap();
        bloom::probe(&words, &hashes, &rows, &mut found).unwrap();
        assert_eq!(found_bits, both, "the bits of a key were lost");
    });
}

#[test]
fn participants_add_to_one_word_and_lose_no_bit() {
    participants_add_to_one_word(false);
}

#[test]
#[should_panic(expected = "the bits of a key were lost")]
fn a_plain_read_and_write_loses_the_bits_of_a_filter() {
    participants_add_to_one_word(true);
}

/// Two participants mark records 0 and 1 of a chunk of three at once; once
/// both are done, as the last participant to leave the table walks it,
/// only record 2 is without a mark.
fn participants_mark_one_word(plain: bool) {
    const SIZE: usize = 24;
    const LEN: usize = CHUNK_HEADER + 3 * SIZE;
    ::loom::model(move || {
        let marks = Arc::new(LoomWord::new(plain));
        let threads: Vec<_> = (0..2)
            .map(|record| {
                let marks = marks.clone();
                thread::spawn(move || {
                    let one = [1];
                    let rows = RowMaskView::try_new(1, &one).unwrap();
                    let refs = [Place {
                        chunk: 0,
                        byte: CHUNK_HEADER + record * SIZE,
                    }
                    .reference()];
                    marks::mark(&[LEN], SIZE, &refs, &rows, &*marks).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // The walk reads the used mark of a chunk of three records.
        let mut chunk = [0_u64; LEN / 8];
        chunk[0] = LEN as u64;
        let bases = [chunk.as_mut_ptr().cast::<u8>()];
        // SAFETY: the chunk is this array, read only by the walk below.
        let chunks = unsafe { Chunks::new(&bases, &[LEN]) }.unwrap();
        let mut out = [0; 4];
        let count =
            marks::scan_unmarked(&chunks, SIZE, &mut Cursor::start(), Some(&*marks), &mut out)
                .unwrap();
        assert_eq!(
            out[..count],
            [Place {
                chunk: 0,
                byte: CHUNK_HEADER + 2 * SIZE
            }
            .reference()],
            "a mark was lost"
        );
    });
}

#[test]
fn participants_mark_one_word_and_lose_no_mark() {
    participants_mark_one_word(false);
}

#[test]
#[should_panic(expected = "a mark was lost")]
fn a_plain_read_and_write_loses_marks() {
    participants_mark_one_word(true);
}
