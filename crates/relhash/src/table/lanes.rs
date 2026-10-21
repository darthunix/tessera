//! The vertical probe of a word of rows, which a probe and a find or
//! insert share: the rows of a word take each step together, so that
//! their loads are in flight at once.
//!
//! Its prefetch hint: `prfm pldl1keep` on AArch64,
//! nothing elsewhere (other targets and Miri), in the manner of the hash
//! kernels' `bulk_op`. The table keeps its own hint, so that it needs no
//! module of the kernels; on x86-64 it would be `_mm_prefetch(_MM_HINT_T0)`.
//!
//! Array comparisons in masks were tried and removed: the probe gathers a
//! candidate's fields with scalar loads from scattered records into
//! arrays, and vector loads of arrays just filled by 32-bit stores cannot
//! take their data from those stores; with the comparisons the probe of a
//! table in L1 took twice the cycles of one that compares each field as
//! it is read.

#[cfg(all(target_arch = "aarch64", not(miri)))]
use core::arch::asm;
use core::mem::MaybeUninit;

use crate::error::{Kind, Result};
use crate::ones;

use super::keys::WordKeys;
use super::record::{Access, LONG_CHAIN};
use super::region::Region;

/// Ask for the cache line at `address` to be loaded for reading. A hint:
/// it never faults, whatever the address, and changes no state a program
/// can observe.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline(always)]
pub(super) fn prefetch(address: *const u8) {
    // SAFETY: `prfm` reads no memory architecturally and does not fault
    // on any address; it touches no register but its operand.
    unsafe {
        asm!(
            "prfm pldl1keep, [{address}]",
            address = in(reg) address,
            options(nostack, preserves_flags, readonly)
        );
    }
}

/// A prefetch hint where none is implemented: nothing.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(always)]
pub(super) fn prefetch(_address: *const u8) {}

/// Rows of a word from which a probe goes vertically, phase by phase over
/// the word, rather than row by row: fewer rows do not pay for phases
/// that run over all 64.
pub(super) const VERTICAL_MIN_ROWS: u32 = 8;

/// The per-row arrays of a word's vertical probe, indexed by row within
/// the word. A probe writes a row's entries before it reads them, for the
/// selected rows only, so the arrays start uninitialized and are never
/// cleared.
pub(super) struct Lanes<S: Copy> {
    /// The row's hash.
    hash: [MaybeUninit<u32>; 64],
    /// The reference of the record the row's walk is at: first its
    /// bucket's head, then the next of each record that did not match.
    current: [MaybeUninit<u32>; 64],
    /// Where that record lies, found once a step for both the prefetch
    /// hint and the comparison.
    spot: [MaybeUninit<S>; 64],
}

impl<S: Copy> Lanes<S> {
    #[inline(always)]
    pub(super) fn new() -> Self {
        Self {
            hash: [MaybeUninit::uninit(); 64],
            current: [MaybeUninit::uninit(); 64],
            spot: [MaybeUninit::uninit(); 64],
        }
    }
}

/// Probe the `selected` rows of one word phase by phase: every row's bucket
/// is hinted to the cache, then every head read; while rows remain, every
/// candidate record is checked and hinted, then every candidate compared
/// with its row and the rows that did not match moved down their chains.
/// The loads of different rows are independent and in flight together,
/// which a row-by-row walk, with its longer path per row, allows for fewer
/// rows at a time. A candidate is compared field by field as it is read:
/// gathering the fields into arrays for a vector comparison made vector
/// loads wait on the scalar stores that had just filled them. The answer is the
/// row-by-row one: each row gets the first record of its chain with its
/// hash, null bits and keys. Returns the rows found.
#[inline(always)]
pub(super) fn probe_word<R: Region, const N: usize>(
    access: &mut Access<'_, R>,
    word_keys: &WordKeys<'_>,
    hashes: &[u32],
    selected: u64,
    matches: &mut [u32],
    lanes: &mut Lanes<R::Spot>,
) -> Result<u64> {
    for bit in ones(selected) {
        let hash = hashes[bit];
        lanes.hash[bit].write(hash);
        access.prefetch_bucket(hash);
    }
    // SAFETY (every `assume_init` below): the entries of a row are read
    // only for rows of `selected` (`pending` is a subset of it), after the
    // loop above wrote its hash and the loop below its first offset.
    let mut pending = 0;
    for bit in ones(selected) {
        let head = access.head(unsafe { lanes.hash[bit].assume_init() });
        lanes.current[bit].write(head);
        pending |= u64::from(head != 0) << bit;
    }
    let mut hits = 0;
    let mut steps = 0;
    // One compare a round, as the bound alone took: the record count or a
    // long chain, whichever comes first.
    let mut limit = access.records().min(LONG_CHAIN);
    while pending != 0 {
        if steps == limit {
            if steps == LONG_CHAIN {
                // A copy of the access: given it, the loops would keep it
                // in memory, not in registers.
                return finish_alone::<R, N>(
                    *access, word_keys, lanes, pending, hits, matches, steps,
                );
            }
            access.check_steps(steps)?;
            limit = access.records().min(LONG_CHAIN);
        }
        steps += 1;
        for bit in ones(pending) {
            let place = access.place(unsafe { lanes.current[bit].assume_init() }, Kind::Damaged)?;
            // SAFETY: `place` accepted it.
            let spot = unsafe { access.spot(place) };
            lanes.spot[bit].write(spot);
            access.prefetch_record(spot);
        }
        let mut rest = 0;
        for bit in ones(pending) {
            let offset = unsafe { lanes.current[bit].assume_init() };
            // SAFETY: `place` accepted every offset of `pending` above,
            // and the loop above resolved its spot.
            let record = unsafe { access.open_at(lanes.spot[bit].assume_init(), offset) }?;
            // SAFETY: the record is this table's, whose key count the
            // buffer was made for and `N` is 0 or.
            let hash = unsafe { lanes.hash[bit].assume_init() };
            if record.hash() == hash && unsafe { word_keys.equal::<N>(bit, &record) } {
                matches[bit] = offset;
                hits |= 1 << bit;
            } else {
                let next = record.next();
                lanes.current[bit].write(next);
                rest |= u64::from(next != 0) << bit;
            }
        }
        pending = rest;
    }
    Ok(hits)
}

/// The rows of a word still walking after [`LONG_CHAIN`] rounds, which are
/// rare and may go round a cycle: each walks on alone, where a walk stops
/// at one. Returns the rows found, `hits` with them.
#[cold]
#[inline(never)]
fn finish_alone<R: Region, const N: usize>(
    access: Access<'_, R>,
    word_keys: &WordKeys<'_>,
    lanes: &Lanes<R::Spot>,
    pending: u64,
    mut hits: u64,
    matches: &mut [u32],
    steps: u64,
) -> Result<u64> {
    for bit in ones(pending) {
        // SAFETY: the rounds wrote the hash and the next offset of every
        // row of `pending`.
        let (offset, hash) = unsafe {
            (
                lanes.current[bit].assume_init(),
                lanes.hash[bit].assume_init(),
            )
        };
        // SAFETY: the records are this table's, whose key count the buffer
        // was made for and `N` is 0 or.
        let found = access.find_long(offset, hash, steps, |record| unsafe {
            word_keys.equal::<N>(bit, record)
        })?;
        if found != 0 {
            matches[bit] = found;
            hits |= 1 << bit;
        }
    }
    Ok(hits)
}
