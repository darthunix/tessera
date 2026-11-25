//! The proofs by Kani of the fast loop's bounds, which `cargo kani -p
//! tessera-parquet` runs: one turn of two elements, from any state its
//! guard admits, over any bytes, reads and writes inside its buffers and
//! leaves a state the next turn's assumption admits, its waiting copy's
//! source among them, and the copy after the loop, made as `fast` makes
//! it, stays inside them too; and the copy of a pattern writes only its
//! 64 bytes, for every offset up to 64. That the loop starts a turn only
//! where the first proof's state holds, the tests see: a debug assertion
//! checks it before every turn.
//!
//! The buffers are small and the positions in them bounded. A turn reads
//! and writes only at its positions: up to 66 bytes of input past `ip`,
//! 64 bytes of output past the frontier, and a copy's 64 bytes from a
//! source behind the frontier or behind `ip`. Where the positions stand
//! in their buffers does not change what it touches relative to them, so
//! what the proof needs is the room past them, which the guard fixes, and
//! a source behind them, which the assumption fixes; an offset of a copy
//! larger than the model's output is refused by the same comparison as
//! one before its start.
//!
//! Kani reads the plain copy of a pattern: the shuffles of NEON and SSSE3
//! are cut out under it.

use super::{Fast, SLOP, pattern64, step};

/// The input: the 130 bytes the loop's guard leaves past `ip`, and room
/// for `ip` to stand at 1 to 10.
const IN: usize = 2 * SLOP + 1 + 11;
/// The output: the 128 bytes the guard leaves past the frontier and the
/// waiting copy, and 64 written before it.
const OUT: usize = 2 * SLOP + 64 + 64;

#[kani::proof]
#[kani::unwind(17)]
fn a_turn_of_the_fast_loop_stays_in_its_buffers() {
    let input: [u8; IN] = kani::any();
    let mut out: [u8; OUT] = kani::any();
    let none = [0u8; SLOP];
    let ip: usize = kani::any();
    let op: usize = kani::any();
    let len_from: usize = kani::any();
    let place: usize = kani::any();
    let source: u8 = kani::any();
    // The guard of a turn.
    kani::assume(ip >= 1 && ip < IN - (2 * SLOP + 1));
    kani::assume(len_from <= SLOP && op < OUT);
    kani::assume(op + len_from < OUT - (2 * SLOP - 1));
    let ib = input.as_ptr();
    let ob = out.as_mut_ptr();
    // The copy that waits: none; a literal's bytes, before `ip`; or bytes
    // written, ending at or before `op`.
    let from = match source {
        0 => {
            kani::assume(len_from == 0);
            none.as_ptr()
        }
        1 => {
            kani::assume(place < ip);
            // SAFETY: `place` lies in the input.
            unsafe { ib.add(place) }
        }
        _ => {
            kani::assume(place < OUT && place + len_from <= op);
            // SAFETY: `place` lies in the output.
            unsafe { ob.add(place).cast_const() }
        }
    };
    let mut s = Fast {
        // SAFETY: `ip` lies in the input.
        ip: unsafe { ib.add(ip) },
        tag: usize::from(input[ip - 1]),
        op,
        from,
        len_from,
    };
    // SAFETY: the state the guard admits, which the proof is about.
    let both = unsafe { step(&mut s, ob, none.as_ptr()) && step(&mut s, ob, none.as_ptr()) };
    // The turn moves `ip` at most 61 twice, and the frontier with the
    // waiting copy at most 64 twice.
    // SAFETY: `ip` stays in the input, which the bounds checks show.
    let moved = unsafe { s.ip.offset_from_unsigned(ib) };
    assert!(moved >= ip && moved <= ip + 122);
    assert!(s.len_from <= SLOP);
    assert!(s.op + s.len_from <= op + len_from + 2 * SLOP);
    // A copy that waits when the loop stops here has its 64 bytes; a
    // copy of a pattern wrote its own and leaves none waiting.
    assert!(s.len_from == 0 || s.op + SLOP <= OUT);
    // The waiting copy's source is one the next turn may assume: none, a
    // literal's bytes before `ip`, or bytes written that end at or before
    // the frontier, each with its 64 bytes.
    let (at, i0, o0) = (s.from as usize, ib as usize, ob as usize);
    assert!(
        (s.len_from == 0 && at == none.as_ptr() as usize)
            || (at >= i0 && at < i0 + moved && at + SLOP <= i0 + IN)
            || (at >= o0 && at + s.len_from <= o0 + s.op && at + SLOP <= o0 + OUT)
    );
    // The copy after the loop, as `fast` makes it.
    if s.len_from != 0 {
        // SAFETY: what the assertions above show.
        unsafe { super::copy64(ob.add(s.op), s.from, s.len_from) };
    }
    kani::cover!(both, "two elements decoded");
    kani::cover!(!both, "an element left to the careful loop");
}

#[kani::proof]
#[kani::unwind(17)]
fn the_copy_of_a_pattern_writes_its_64_bytes_only() {
    let mut out: [u8; 2 * SLOP + 1] = kani::any();
    let before = out;
    let offset: usize = kani::any();
    kani::assume(offset <= SLOP);
    let dst = 2 * SLOP + 1 - SLOP;
    kani::assume(offset <= dst);
    // SAFETY: `[dst - offset, dst + 64)` lies in `out`.
    let done = unsafe { pattern64(out.as_mut_ptr().add(dst), offset) };
    assert_eq!(done, offset != 0);
    let j: usize = kani::any();
    kani::assume(j < dst);
    assert_eq!(out[j], before[j]);
    if done {
        let i: usize = kani::any();
        kani::assume(i < SLOP);
        assert_eq!(out[dst + i], out[dst + i - offset]);
    }
}
