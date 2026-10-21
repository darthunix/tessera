//! The aggregate states of a grouping in the payloads of a table's
//! records: `count(*)` and `count(x)`, the folds of int2, int4 and int8
//! values, the sums and extremes of numeric, over the rows of a batch;
//! and the merge of groups read back from disk, by the kinds of state
//! they hold. They reach the payloads only through what the table offers
//! its writer: the cursor over payloads and the merge by a rule.
//!
//! A call that is not generic is a public function over a private body,
//! as the table's methods were over these bodies: LLVM then optimizes the
//! body alone and again where it is inlined, and the loops compile as they
//! did inside the table; a body made public itself compiles otherwise.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView, ones};

use crate::decimal::{self, ExtremeState, Offer, Partial, Partials, SumState, Term, Terms};
use crate::ops::ArithmeticError;
use crate::table::{CombineStop, HELD, TableMut};

/// How an aggregate's state in a payload takes a row's value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fold {
    /// The sum, as an `i64`; an overflow fails with
    /// [`ArithmeticError::BigintOutOfRange`], as PostgreSQL's transition.
    Sum,
    /// The least value.
    Min,
    /// The greatest value.
    Max,
}

/// Where an aggregate keeps its state in a payload: an `i64` at byte
/// `value_at`, and whether it has seen a value as bit `flag_bit` of the
/// `u64` at byte `flags_at`, which several aggregates may share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slot {
    pub value_at: usize,
    pub flags_at: usize,
    pub flag_bit: u32,
}

/// Reject offsets, a mask or payload words that do not fit a payload of
/// `payload_size` bytes: a fold passes the size, not the table, so that the
/// table's address stays out of the call and its fields in registers across
/// the fold's loop.
#[inline(always)]
fn check_words(payload_size: usize, offsets: usize, nrows: usize, words: &[usize]) -> Result<()> {
    ensure!(
        offsets == nrows,
        "the offsets and mask of the batch have different row counts"
    );
    for &at in words {
        ensure!(
            at.is_multiple_of(8) && at.checked_add(8).is_some_and(|end| end <= payload_size),
            "a payload word at byte {at} is past the payload of {} bytes",
            payload_size
        );
    }
    Ok(())
}

#[inline(always)]
fn read_word(payload: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&payload[at..at + 8]);
    u64::from_ne_bytes(word)
}

#[inline(always)]
fn write_word(payload: &mut [u8], at: usize, value: u64) {
    payload[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

/// Add one, as an `i64` at byte `at`, to the payload of each selected
/// row's record: `count(*)`.
pub fn count_rows(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    at: usize,
) -> Result<()> {
    count_rows_inner(table, offsets, rows, at)
}

fn count_rows_inner(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    at: usize,
) -> Result<()> {
    let nrows = rows.nrows();
    check_words(table.payload_size(), offsets.len(), nrows, &[at])?;
    let mut payloads = table.payloads();
    for index in 0..nrows.div_ceil(64) {
        let bits = rows.word(index);
        for bit in ones(bits) {
            let row = index * 64 + bit;
            let payload = payloads.payload(offsets[row])?;
            let count = (read_word(payload, at) as i64)
                .checked_add(1)
                .ok_or(ArithmeticError::BigintOutOfRange)?;
            write_word(payload, at, count as u64);
        }
    }
    Ok(())
}

/// Add one, as an `i64` at byte `at`, for each selected row whose value
/// is not NULL: `count(x)`, which reads the NULL flags alone.
pub fn count_values<C: ColumnReader + ?Sized>(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    column: &C,
    at: usize,
) -> Result<()> {
    let nrows = rows.nrows();
    check_words(table.payload_size(), offsets.len(), nrows, &[at])?;
    ensure!(
        column.nrows() == nrows,
        "column and selection row counts differ"
    );
    let mut payloads = table.payloads();
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        for (row, value) in column.word_values(index, selected)? {
            if value.is_none() {
                continue;
            }
            let payload = payloads.payload(offsets[row])?;
            let count = (read_word(payload, at) as i64)
                .checked_add(1)
                .ok_or(ArithmeticError::BigintOutOfRange)?;
            write_word(payload, at, count as u64);
        }
    }
    Ok(())
}

/// Fold each selected row's non-NULL value into the state at `slot` of
/// its record's payload: the first value marks the state as seen, so a
/// group without one stays NULL, as PostgreSQL's strict transitions.
/// Rows go in row order, so an overflow fails where the row-wise sum
/// would.
pub fn fold<C, V>(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    column: &C,
    fold: Fold,
    slot: Slot,
) -> Result<()>
where
    C: ColumnReader<Value = V> + ?Sized,
    V: Into<i64> + Copy,
{
    let nrows = rows.nrows();
    check_words(
        table.payload_size(),
        offsets.len(),
        nrows,
        &[slot.value_at, slot.flags_at],
    )?;
    ensure!(
        slot.flag_bit < 64,
        "flag bit {} is past a word",
        slot.flag_bit
    );
    ensure!(
        column.nrows() == nrows,
        "column and selection row counts differ"
    );
    let flag = 1u64 << slot.flag_bit;
    let mut payloads = table.payloads();
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        for (row, value) in column.word_values(index, selected)? {
            let Some(value) = value else { continue };
            let value: i64 = value.into();
            let payload = payloads.payload(offsets[row])?;
            let flags = read_word(payload, slot.flags_at);
            let state = read_word(payload, slot.value_at) as i64;
            let next = if flags & flag == 0 {
                value
            } else {
                match fold {
                    Fold::Sum => state
                        .checked_add(value)
                        .ok_or(ArithmeticError::BigintOutOfRange)?,
                    Fold::Min => state.min(value),
                    Fold::Max => state.max(value),
                }
            };
            write_word(payload, slot.value_at, next as u64);
            write_word(payload, slot.flags_at, flags | flag);
        }
    }
    Ok(())
}

/// The most sums [`sum_terms`] folds in a call.
pub const MAX_SUMS: usize = 32;

/// A sum or an average whose [`SumState`] a record keeps at byte `at` of
/// its payload, the terms of its rows ([`Terms`], or their partial states,
/// [`Partials`]), and the mask of the rows it leaves to the caller.
pub struct SumSlot<'a, T: ?Sized> {
    /// The rows' terms or partial states.
    pub terms: &'a T,
    /// The state's first byte in the payload.
    pub at: usize,
    /// The rows the state does not take; every word is written.
    pub rest: RowMask<'a>,
}

/// Groups a word of rows folds first among themselves, before their
/// records: past them, a word's rows go to their records one by one.
const LOCAL_GROUPS: usize = 16;

/// Slots of the hash of a word's groups: twice the groups, so that a probe
/// ends.
const LOCAL_SLOTS: usize = 2 * LOCAL_GROUPS;

const _: () = assert!(LOCAL_GROUPS <= HELD, "a cursor holds every group of a word");

/// The decimals one group's rows of a word gave one sum, added up: their
/// sum at their scale and how many.
#[derive(Clone, Copy, Debug, Default)]
struct Local {
    value: i128,
    count: u64,
    scale: u32,
}

/// The group of a word's record `offset`, a new one while there is room;
/// `None` once the word has [`LOCAL_GROUPS`] groups and this is another.
#[inline(always)]
fn local_group(
    slots: &mut [u8; LOCAL_SLOTS],
    groups: &mut [u32; LOCAL_GROUPS],
    ngroups: &mut usize,
    offset: u32,
) -> Option<usize> {
    let mut slot =
        (offset.wrapping_mul(0x9E37_79B1) >> (32 - LOCAL_SLOTS.trailing_zeros())) as usize;
    loop {
        let entry = slots[slot];
        if entry == 0 {
            if *ngroups == LOCAL_GROUPS {
                return None;
            }
            groups[*ngroups] = offset;
            *ngroups += 1;
            slots[slot] = *ngroups as u8;
            return Some(*ngroups - 1);
        }
        if groups[usize::from(entry) - 1] == offset {
            return Some(usize::from(entry) - 1);
        }
        slot = (slot + 1) % LOCAL_SLOTS;
    }
}

/// Fold one term into the [`SumState`] at byte `at` of a payload: false
/// when the state does not take it.
#[inline(always)]
fn fold_term(payload: &mut [u8], at: usize, term: Term) -> Result<bool> {
    // One check for the four words, then words at fixed places.
    let slots: &mut [u8; 8 * SumState::WORDS] =
        (&mut payload[at..at + 8 * SumState::WORDS]).try_into()?;
    let mut words: [u64; SumState::WORDS] = std::array::from_fn(|word| read_word(slots, 8 * word));
    let taken = SumState::add_to(&mut words, term);
    if taken {
        for (word, value) in words.into_iter().enumerate() {
            write_word(slots, 8 * word, value);
        }
    }
    Ok(taken)
}

/// Fold each selected row's terms into the [`SumState`]s of its record's
/// payload, as if row by row: a decimal into the sum, NaN or an infinity
/// into its flag, NULL skipped. A row a state does not take (a longer
/// value, a decimal the sum refuses) goes to that sum's rest, for the
/// caller to add by the core's means.
///
/// A word's rows are first added up by group, for up to `LOCAL_GROUPS`
/// groups a word, whose records are found once: for each sum, the decimals
/// of one scale a group's rows give make one local sum, which goes into the
/// record once. The rows a source hands over in bulk (decimals of one scale
/// without NULL, integers) are added by a loop over their values alone; the
/// others go term by term, and a term that is no such decimal goes to the
/// record directly, as do the rows of further groups. Exact sums add in any
/// order; a local sum the state refuses at its bound is taken again row by
/// row, so that the rows it refuses go to the rest.
pub fn sum_terms<T: Terms>(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    sums: &mut [SumSlot<'_, T>],
) -> Result<()> {
    let nrows = rows.nrows();
    let nsums = sums.len();
    ensure!(nsums <= MAX_SUMS, "{nsums} sums in a call, past {MAX_SUMS}");
    ensure!(
        offsets.len() == nrows,
        "the offsets and mask of the batch have different row counts"
    );
    for sum in sums.iter() {
        let words: [usize; SumState::WORDS] = std::array::from_fn(|word| sum.at + 8 * word);
        check_words(table.payload_size(), offsets.len(), nrows, &words)?;
        ensure!(
            sum.rest.as_view().nrows() == nrows,
            "the rest and the selection of a sum have different row counts"
        );
    }
    let mut payloads = table.payloads();
    let mut others = [0_u64; MAX_SUMS];
    let mut groups = [0_u32; LOCAL_GROUPS];
    let mut locals = [Local::default(); LOCAL_GROUPS];
    let mut row_groups = [0_u8; 64];
    let mut group_rows = [0_u64; LOCAL_GROUPS];
    let mut bulk = [0_i128; LOCAL_GROUPS];
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index);
        // The word's groups, and the rows of the groups that fit.
        let mut slots = [0_u8; LOCAL_SLOTS];
        let mut ngroups = 0;
        let mut local_rows = 0_u64;
        let look = selected;
        for bit in ones(look) {
            match local_group(
                &mut slots,
                &mut groups,
                &mut ngroups,
                offsets[index * 64 + bit],
            ) {
                Some(group) => {
                    row_groups[bit] = group as u8;
                    local_rows |= 1 << bit;
                }
                None => break,
            }
        }
        group_rows[..ngroups].fill(0);
        let look = local_rows;
        for bit in ones(look) {
            group_rows[usize::from(row_groups[bit]) % LOCAL_GROUPS] |= 1 << bit;
        }
        for (group, &offset) in groups[..ngroups].iter().enumerate() {
            payloads.hold(group, offset)?;
        }
        for (sum, other) in sums.iter().zip(others.iter_mut()) {
            *other = 0;
            locals[..ngroups].fill(Local::default());
            let mut taken = 0_u64;
            // The rows handed over in bulk: their values alone, added up by
            // group as the source reads them, counted by the groups' rows.
            // At most 64 values of an i64 each: no group's bulk sum comes
            // near the range of an i128, so it is added unchecked; the bound
            // of the state is checked when the sum is taken into it.
            bulk[..ngroups].fill(0);
            if let Some(word) = sum.terms.fold_decimals(index, local_rows, |bit, value| {
                bulk[usize::from(row_groups[bit % 64]) % LOCAL_GROUPS] += i128::from(value);
            }) {
                ensure!(
                    word.rows & !local_rows == 0 && word.scale <= decimal::MAX_READ_SCALE,
                    "a source handed over rows it was not asked for"
                );
                for ((local, &value), &rows) in locals[..ngroups]
                    .iter_mut()
                    .zip(&bulk[..ngroups])
                    .zip(&group_rows[..ngroups])
                {
                    *local = Local {
                        value,
                        count: u64::from((rows & word.rows).count_ones()),
                        scale: word.scale,
                    };
                }
                taken = word.rows;
            }
            // The other rows of the word's groups, term by term.
            let look = local_rows & !taken;
            for bit in ones(look) {
                let group = usize::from(row_groups[bit]);
                let term = sum.terms.term(index * 64 + bit);
                let local = &mut locals[group];
                match term {
                    Term::Null => {}
                    Term::Decimal(decimal)
                        if decimal.scale() <= decimal::MAX_READ_SCALE
                            && (local.count == 0 || local.scale == decimal.scale()) =>
                    {
                        local.value += i128::from(decimal.value());
                        local.count += 1;
                        local.scale = decimal.scale();
                        taken |= 1 << bit;
                    }
                    _ => {
                        let payload = payloads.held(group)?;
                        if !fold_term(payload, sum.at, term)? {
                            *other |= 1 << bit;
                        }
                    }
                }
            }
            // Each group's local sum into its record.
            for (group, local) in locals[..ngroups].iter().enumerate() {
                if local.count == 0 {
                    continue;
                }
                let payload = payloads.held(group)?;
                let slots: &mut [u8; 8 * SumState::WORDS] =
                    (&mut payload[sum.at..sum.at + 8 * SumState::WORDS]).try_into()?;
                let mut words: [u64; SumState::WORDS] =
                    std::array::from_fn(|word| read_word(slots, 8 * word));
                if SumState::add_many_to(&mut words, local.value, local.scale, local.count) {
                    for (word, value) in words.into_iter().enumerate() {
                        write_word(slots, 8 * word, value);
                    }
                    continue;
                }
                // At the bound: the rows that made the local sum, one by one;
                // one that is no decimal of its scale goes to the rest.
                let again = taken;
                for bit in ones(again) {
                    if usize::from(row_groups[bit]) != group {
                        continue;
                    }
                    let term = sum.terms.term(index * 64 + bit);
                    let decimal =
                        matches!(term, Term::Decimal(decimal) if decimal.scale() == local.scale);
                    if !decimal || !fold_term(payload, sum.at, term)? {
                        *other |= 1 << bit;
                    }
                }
            }
        }
        // The rows of further groups, each to its record.
        let look = selected & !local_rows;
        for bit in ones(look) {
            let row = index * 64 + bit;
            let payload = payloads.payload(offsets[row])?;
            for (sum, other) in sums.iter().zip(others.iter_mut()) {
                let term = sum.terms.term(row);
                if !matches!(term, Term::Null) && !fold_term(payload, sum.at, term)? {
                    *other |= 1 << bit;
                }
            }
        }
        for (sum, &other) in sums.iter_mut().zip(others.iter()) {
            sum.rest.set_word(index, other)?;
        }
    }
    Ok(())
}

/// Merge each selected row's partial states into the [`SumState`]s of its
/// record's payload ([`SumState::merge`]), the record found once a row:
/// NULL skipped, a state left to the caller ([`Partial::Other`]) or one
/// whose sum the record's refuses at its bound set in that sum's rest, the
/// record's state unchanged. A final grouping's rows come a group's partial
/// state from each participant, the groups of a batch from one: its rows
/// are not added up by group first, as [`sum_terms`] adds up terms.
pub fn sum_partials<P: Partials>(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    sums: &mut [SumSlot<'_, P>],
) -> Result<()> {
    let nrows = rows.nrows();
    let nsums = sums.len();
    ensure!(nsums <= MAX_SUMS, "{nsums} sums in a call, past {MAX_SUMS}");
    for sum in sums.iter() {
        let words: [usize; SumState::WORDS] = std::array::from_fn(|word| sum.at + 8 * word);
        check_words(table.payload_size(), offsets.len(), nrows, &words)?;
        ensure!(
            sum.rest.as_view().nrows() == nrows,
            "the rest and the selection of a sum have different row counts"
        );
    }
    let mut payloads = table.payloads();
    let mut others = [0_u64; MAX_SUMS];
    for index in 0..nrows.div_ceil(64) {
        others[..nsums].fill(0);
        let look = rows.word(index);
        for bit in ones(look) {
            let row = index * 64 + bit;
            let payload = payloads.payload(offsets[row])?;
            for (sum, other) in sums.iter().zip(others.iter_mut()) {
                let state = match sum.terms.partial(row)? {
                    Partial::Null => continue,
                    Partial::State(state) => state,
                    Partial::Other => {
                        *other |= 1 << bit;
                        continue;
                    }
                };
                let slots: &mut [u8; 8 * SumState::WORDS] =
                    (&mut payload[sum.at..sum.at + 8 * SumState::WORDS]).try_into()?;
                let mut ours =
                    SumState::from_words(std::array::from_fn(|word| read_word(slots, 8 * word)));
                if !ours.merge(state) {
                    *other |= 1 << bit;
                    continue;
                }
                for (word, value) in ours.to_words().into_iter().enumerate() {
                    write_word(slots, 8 * word, value);
                }
            }
        }
        for (sum, &other) in sums.iter_mut().zip(others.iter()) {
            sum.rest.set_word(index, other)?;
        }
    }
    Ok(())
}

/// A `min` or `max` of numeric of [`extremes`]: its rows' terms, its
/// state in the payload, and the rows it leaves to the caller.
pub struct ExtremeSlot<'a, T: ?Sized> {
    /// The rows' terms.
    pub terms: &'a T,
    /// The state's first byte in the payload ([`ExtremeState`]).
    pub at: usize,
    /// `max`, or `min` when false.
    pub max: bool,
    /// The rows the state leaves to the caller; every word is written.
    pub rest: RowMask<'a>,
}

/// Offer each selected row's term to the `min` or `max` state of its
/// record's payload ([`ExtremeState::offer`]), in the rows' order: the
/// rows the state does not decide go to the slot's rest, and the group's
/// later rows of the batch after them, for the caller to take in order.
pub fn extremes<T: Terms>(
    table: &mut TableMut<'_>,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    slot: &mut ExtremeSlot<'_, T>,
) -> Result<()> {
    let nrows = rows.nrows();
    let words: [usize; ExtremeState::WORDS] = std::array::from_fn(|word| slot.at + 8 * word);
    check_words(table.payload_size(), offsets.len(), nrows, &words)?;
    ensure!(
        slot.rest.as_view().nrows() == nrows,
        "the rest and the selection of an extreme have different row counts"
    );
    let mut payloads = table.payloads();
    for index in 0..nrows.div_ceil(64) {
        let mut other = 0_u64;
        for bit in ones(rows.word(index)) {
            let row = index * 64 + bit;
            let term = slot.terms.term(row);
            if term == Term::Null {
                continue;
            }
            let payload = payloads.payload(offsets[row])?;
            let (value, flags) = (read_word(payload, slot.at), read_word(payload, slot.at + 8));
            let mut state = ExtremeState::from_words(value, flags)?;
            match state.offer(term, slot.max) {
                Offer::Kept => continue,
                Offer::Taken => {}
                Offer::Rest => other |= 1 << bit,
            }
            let (value, flags) = state.to_words();
            write_word(payload, slot.at, value);
            write_word(payload, slot.at + 8, flags);
        }
        slot.rest.set_word(index, other)?;
    }
    Ok(())
}

/// How an aggregate's state of a record merges into the state of the same
/// group in another: the payload holds a word of flags, bit `i` set once
/// aggregate `i` has a value, then a word per aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Combine {
    /// A count: the two add, an overflow failing as the transition would.
    Count,
    /// A sum: the two add when both have a value, else the one that has.
    Sum,
    /// The least of the two values.
    Min,
    /// The greatest of the two values.
    Max,
}

/// Merge the records of chunk `source` from byte `*from` on, each the
/// states of a group after a word of flags, into the table by
/// [`TableMut::merge`]: the record of the same keys takes the states in,
/// each as `combines` says, and a group the table lacks is copied whole
/// to chunk `chunk`. A count that overflows an `i64` fails, as the
/// transition would.
pub fn combine(
    table: &mut TableMut<'_>,
    source: usize,
    from: &mut usize,
    chunk: usize,
    combines: &[Combine],
) -> Result<(usize, CombineStop)> {
    combine_inner(table, source, from, chunk, combines)
}

fn combine_inner(
    table: &mut TableMut<'_>,
    source: usize,
    from: &mut usize,
    chunk: usize,
    combines: &[Combine],
) -> Result<(usize, CombineStop)> {
    ensure!(
        combines.len() <= 64 && 8 * (1 + combines.len()) <= table.payload_size(),
        "{} aggregates do not fit a payload of {} bytes with a word of flags",
        combines.len(),
        table.payload_size()
    );
    table.merge(source, from, chunk, |incoming, payload| {
        let theirs_flags = read_word(incoming, 0);
        let mut flags = read_word(payload, 0);
        for (index, &how) in combines.iter().enumerate() {
            let at = 8 * (1 + index);
            let flag = 1u64 << index;
            let theirs = read_word(incoming, at) as i64;
            let ours = read_word(payload, at) as i64;
            let next = match how {
                Combine::Count => ours
                    .checked_add(theirs)
                    .ok_or(ArithmeticError::BigintOutOfRange)?,
                _ if theirs_flags & flag == 0 => continue,
                _ if flags & flag == 0 => theirs,
                Combine::Sum => ours
                    .checked_add(theirs)
                    .ok_or(ArithmeticError::BigintOutOfRange)?,
                Combine::Min => ours.min(theirs),
                Combine::Max => ours.max(theirs),
            };
            if how != Combine::Count {
                flags |= flag;
            }
            write_word(payload, at, next as u64);
        }
        write_word(payload, 0, flags);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use tessera_core::{ColumnView, RowMask, RowMaskView};

    use super::*;
    use crate::column_keys::KeyColumns;
    use crate::int32::murmurhash32;
    use crate::table::{Batch, CHUNK_HEADER, OwnedTable, TableConfig};

    /// Keys and their hashes as a batch's key column.
    fn key_batch(keys: &[i32]) -> (Vec<u32>, Vec<u64>) {
        let hashes = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
        let mut words = vec![0; keys.len().div_ceil(64)];
        for row in 0..keys.len() {
            words[row / 64] |= 1 << (row % 64);
        }
        (hashes, words)
    }

    /// A table of the keys, each with the states `of` gives it.
    fn grouped(
        keys: &[i32],
        of: impl Fn(i32) -> [i64; 5],
        capacity: u64,
    ) -> (OwnedTable, Vec<u32>) {
        let config = TableConfig {
            nkeys: 1,
            payload_size: 40,
        };
        let mut table = OwnedTable::new(&config, capacity, CHUNK_HEADER + 16 * 56).unwrap();
        let column = [ColumnView::try_new(keys, None).unwrap()];
        let (hashes, rows) = key_batch(keys);
        let mut inserted_words = vec![0; rows.len()];
        let mut offsets = vec![0; keys.len()];
        table
            .find_or_insert(
                &hashes,
                &KeyColumns(&column),
                RowMaskView::try_new(keys.len(), &rows).unwrap(),
                &mut offsets,
                &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
            )
            .unwrap();
        for (row, &key) in keys.iter().enumerate() {
            let mut writer = table.table_mut().unwrap();
            let payload = writer.payload_mut(offsets[row]).unwrap();
            for (index, word) in of(key).iter().enumerate() {
                payload[8 * index..8 * index + 8].copy_from_slice(&word.to_ne_bytes());
            }
        }
        (table, offsets)
    }

    /// The states of a group: flags, count, sum, min, max.
    fn states(table: &OwnedTable, offset: u32) -> [i64; 5] {
        let reader = table.table().unwrap();
        let record = reader.record(offset).unwrap();
        let mut out = [0; 5];
        for (index, word) in out.iter_mut().enumerate() {
            *word =
                i64::from_ne_bytes(record.payload[8 * index..8 * index + 8].try_into().unwrap());
        }
        out
    }

    const COMBINES: [Combine; 4] = [Combine::Count, Combine::Sum, Combine::Min, Combine::Max];

    /// Merge the groups of `keys`, with the states `of` gives them, into
    /// `into`: their records go to chunks of their own in `into`, unlinked,
    /// as the chunks of a table on disk come back, and the kernel merges
    /// those chunks, adding chunks and growing the index as it asks.
    fn merge(into: &mut OwnedTable, keys: &[i32], of: impl Fn(i32) -> [i64; 5]) -> Result<()> {
        let column = [ColumnView::try_new(keys, None)?];
        let columns = KeyColumns(&column);
        let (hashes, mut pending_words) = key_batch(keys);
        let mut refs = vec![0; keys.len()];
        let payload: Vec<u8> = keys
            .iter()
            .flat_map(|&key| of(key))
            .flat_map(i64::to_ne_bytes)
            .collect();
        let mut batch = Batch::new(&hashes, &columns, &mut pending_words, &mut refs)?;
        let mut chunk = into.chunks() - 1;
        let mut sources = Vec::new();
        while batch.pending().selected_count() > 0 {
            let source = into.add_chunk()?;
            into.table_mut()?
                .append(source, Some(&payload), &mut batch)?;
            sources.push(source);
        }
        for source in sources {
            let mut at = CHUNK_HEADER;
            loop {
                let (_, stop) = combine(&mut into.table_mut()?, source, &mut at, chunk, &COMBINES)?;
                match stop {
                    CombineStop::Done => break,
                    CombineStop::ChunkFull => chunk = into.add_chunk()?,
                    CombineStop::IndexFull => {
                        let records = into.table()?.stats().records;
                        into.regrow(records * 2)?;
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn states_of_a_group_merge() {
        // Some groups on both sides, with and without values of their own.
        let ours = |key: i32| {
            let key = i64::from(key);
            let flags = if key % 5 == 0 { 0 } else { 0b1110 };
            [flags, key, key * 10, key, key]
        };
        let theirs = |key: i32| {
            let key = i64::from(key);
            let flags = if key % 3 == 0 { 0 } else { 0b1110 };
            [flags, 1, key, key - 100, key + 100]
        };
        let (mut into, _) = grouped(&(0..50).collect::<Vec<_>>(), ours, 64);
        merge(&mut into, &(25..100).collect::<Vec<_>>(), theirs).unwrap();
        let keys: Vec<i32> = (0..100).collect();
        let column = [ColumnView::try_new(&keys, None).unwrap()];
        let (hashes, rows) = key_batch(&keys);
        let mut inserted_words = vec![0; rows.len()];
        let mut offsets = vec![0; keys.len()];
        into.find_or_insert(
            &hashes,
            &KeyColumns(&column),
            RowMaskView::try_new(keys.len(), &rows).unwrap(),
            &mut offsets,
            &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
        )
        .unwrap();
        assert!(
            inserted_words.iter().all(|&word| word == 0),
            "a group was lost"
        );
        for &key in &keys {
            let (a, b) = (ours(key), theirs(key));
            let (in_a, in_b) = (key < 50, key >= 25);
            let expected = match (in_a, in_b) {
                (true, false) => a,
                (false, true) => b,
                _ => {
                    let mut merged = [a[0] | b[0], a[1] + b[1], a[2], a[3], a[4]];
                    let (fa, fb) = (a[0] != 0, b[0] != 0);
                    if fa && fb {
                        merged[2] = a[2] + b[2];
                        merged[3] = a[3].min(b[3]);
                        merged[4] = a[4].max(b[4]);
                    } else if fb {
                        merged[2..].copy_from_slice(&b[2..]);
                    }
                    merged
                }
            };
            assert_eq!(states(&into, offsets[key as usize]), expected, "key {key}");
        }
    }

    #[test]
    fn a_merged_count_past_the_int8_range_fails() {
        let (mut into, _) = grouped(&[7], |_| [0, i64::MAX, 0, 0, 0], 64);
        let error = merge(&mut into, &[7], |_| [0, 1, 0, 0, 0]).unwrap_err();
        assert!(error.to_string().contains("bigint out of range"), "{error}");
        let (mut table, _) = grouped(&[7], |_| [0; 5], 64);
        let chunk = table.chunks() - 1;
        let mut at = CHUNK_HEADER;
        let error = combine(
            &mut table.table_mut().unwrap(),
            chunk,
            &mut at,
            chunk,
            &COMBINES,
        )
        .unwrap_err();
        let error = error.downcast_ref::<relhash::Error>().unwrap();
        assert_eq!(
            (error.kind(), error.rule()),
            (relhash::Kind::WrongCall, relhash::Rule::MergeIntoItself)
        );
    }
}
