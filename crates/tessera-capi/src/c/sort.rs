//! The sort entry points, declared in `include/tessera/sort.h`.
//!
//! The keys come as `TessSortKey`s, a table key kind and flags, and are
//! decoded into [`SortKey`]s for every call; the table comes as a
//! `TessTableRef`, as for the table's entry points, and the items and
//! references are the caller's arrays.

use std::ffi::{c_int, c_uint};
use std::mem::offset_of;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_kernels::sort::{
    MAX_ITEM_WORDS, MAX_MERGE_RUNS, MERGE_STATE_WORDS, SortKey, item_words, key_lanes, merge,
    sort_items, top_candidates,
};
use tessera_kernels::table::{KeyKind, MAX_KEYS};

use super::mask::Mask;
use super::status::{Code, Status, guard};
use super::table::{TableKey, TableKeys, TableRef, attach, slots, values};

/// `TESS_SORT_DESCENDING`.
pub const DESCENDING: u32 = 0x1;
/// `TESS_SORT_NULLS_FIRST`.
pub const NULLS_FIRST: u32 = 0x2;
/// `TESS_SORT_NULLABLE`.
pub const NULLABLE: u32 = 0x4;

/// `TessSortKey`: one sort key.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CSortKey {
    /// A `TessTableKeyKind`.
    pub kind: c_uint,
    /// `TESS_SORT_*` bits.
    pub flags: u32,
}

/// `tess_sort_layout`: the sizes and offsets of `TessSortKey`, for the C
/// side's checks; 0 for an unknown kind.
#[unsafe(no_mangle)]
pub extern "C" fn tess_sort_layout(kind: c_uint) -> usize {
    match kind {
        0 => size_of::<CSortKey>(),
        1 => offset_of!(CSortKey, flags),
        _ => 0,
    }
}

/// Decode `nkeys` keys into `result`, returning how many.
///
/// # Safety
///
/// `keys` must point to `nkeys` keys when `nkeys` is positive.
unsafe fn sort_keys(
    nkeys: c_int,
    keys: *const CSortKey,
    result: &mut [SortKey; MAX_KEYS],
) -> Result<usize> {
    let nkeys = usize::try_from(nkeys).unwrap_or(usize::MAX);
    ensure!(
        (1..=MAX_KEYS).contains(&nkeys),
        "a sort takes 1 to {MAX_KEYS} keys, not {nkeys}"
    );
    ensure!(!keys.is_null(), "null sort keys");
    // SAFETY: the caller's contract.
    let keys = unsafe { slice::from_raw_parts(keys, nkeys) };
    for (slot, key) in result.iter_mut().zip(keys) {
        ensure!(
            key.flags & !(DESCENDING | NULLS_FIRST | NULLABLE) == 0,
            "unknown sort key flags {:#x}",
            key.flags
        );
        *slot = SortKey {
            kind: match key.kind {
                1 => KeyKind::Int32,
                2 => KeyKind::Int64,
                code => bail!("unknown key kind {code}"),
            },
            descending: key.flags & DESCENDING != 0,
            nulls_first: key.flags & NULLS_FIRST != 0,
            nullable: key.flags & NULLABLE != 0,
        };
    }
    Ok(nkeys)
}

/// A placeholder for the keys not given.
const UNUSED: SortKey = SortKey {
    kind: KeyKind::Int32,
    descending: false,
    nulls_first: false,
    nullable: false,
};

/// `tess_sort_item_words`: the words of one item.
///
/// # Safety
///
/// `keys` as for [`sort_keys`]; `words` must be writable; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort_item_words(
    nkeys: c_int,
    keys: *const CSortKey,
    words: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut decoded = [UNUSED; MAX_KEYS];
            let nkeys = sort_keys(nkeys, keys, &mut decoded)?;
            let out = words.as_mut().context("a null result")?;
            *out = item_words(&decoded[..nkeys])? as c_int;
            Ok(())
        })
    }
}

/// `tess_sort_items`: the item of every record of the table's chunks.
///
/// # Safety
///
/// `table` as for the table's `attach` during the call, with no append
/// running; `keys` as for [`sort_keys`]; `items` must point to `nwords`
/// writable words that nothing else accesses; `count` must be writable;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort_items(
    table: *const TableRef,
    nkeys: c_int,
    keys: *const CSortKey,
    items: *mut u64,
    nwords: usize,
    count: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let mut decoded = [UNUSED; MAX_KEYS];
            let nkeys = sort_keys(nkeys, keys, &mut decoded)?;
            let items = slots(items, nwords, "items")?;
            let out = count.as_mut().context("a null count")?;
            *out = table.sort_items(&decoded[..nkeys], items)? as u64;
            Ok(())
        })
    }
}

/// `tess_sort`: sort the items and write their references in order.
///
/// # Safety
///
/// `items` must point to `nitems * words` writable words and `refs` to
/// `nitems` writable slots, neither accessed by anything else; `status`
/// as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort(
    items: *mut u64,
    nitems: usize,
    words: c_int,
    refs: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let words = usize::try_from(words).context("a negative item width")?;
            let nwords = nitems
                .checked_mul(words)
                .context("items past the address space")?;
            let items = slots(items, nwords, "items")?;
            let refs = slots(refs, nitems, "references")?;
            sort_items(items, words, refs)
        })
    }
}

/// `tess_sort_top_candidates`: keep in `rows` the rows of a batch whose
/// keys beat the top item of a full top-N heap.
///
/// # Safety
///
/// `keys` as for [`sort_keys`]; `table_keys` must point to `nkeys` batch
/// keys, as for the table's entry points, of the mask's rows; `rows` must
/// point to a valid mask that nothing else accesses; `worst` must point to
/// an item's words; `kept` must be writable; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort_top_candidates(
    nkeys: c_int,
    keys: *const CSortKey,
    table_keys: *const TableKey,
    rows: *mut Mask,
    worst: *const u64,
    kept: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut decoded = [UNUSED; MAX_KEYS];
            let nkeys = sort_keys(nkeys, keys, &mut decoded)?;
            let keys = &decoded[..nkeys];
            let mut batch = TableKeys::empty();
            super::table::table_keys(nkeys as c_int, table_keys, &mut batch)?;
            let mut rows = rows.as_mut().context("a null row mask")?.mask()?;
            let worst = values(worst, item_words(keys)?, "worst item")?;
            let count = top_candidates(keys, &batch, &mut rows, worst)?;
            *kept.as_mut().context("a null count")? = count as c_int;
            Ok(())
        })
    }
}

/// `tess_sort_key_lanes`: write the key words of a batch's selected rows
/// into lanes, as a merge compares them.
///
/// # Safety
///
/// `keys` as for [`sort_keys`]; `table_keys` must point to `nkeys` batch
/// keys, as for the table's entry points, of the mask's rows; `rows` must
/// point to a valid mask; `lanes` to `words` pointers, each to `capacity`
/// writable words that nothing else accesses; `count` writable; `status`
/// as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort_key_lanes(
    nkeys: c_int,
    keys: *const CSortKey,
    table_keys: *const TableKey,
    rows: *const Mask,
    words: c_int,
    lanes: *const *mut u64,
    capacity: usize,
    count: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut decoded = [UNUSED; MAX_KEYS];
            let nkeys = sort_keys(nkeys, keys, &mut decoded)?;
            let keys = &decoded[..nkeys];
            let mut batch = TableKeys::empty();
            super::table::table_keys(nkeys as c_int, table_keys, &mut batch)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let words = usize::try_from(words).context("a negative lane count")?;
            ensure!(
                words <= MAX_ITEM_WORDS,
                "an item has at most {MAX_ITEM_WORDS} words, not {words}"
            );
            let pointers = values(lanes, words, "lanes")?;
            let mut storage: [&mut [u64]; MAX_ITEM_WORDS] = Default::default();
            for (index, &pointer) in pointers.iter().enumerate() {
                storage[index] = slots(pointer, capacity, "lane")?;
            }
            let written = key_lanes(keys, &batch, &rows, &mut storage[..words])?;
            *count.as_mut().context("a null count")? = written as c_int;
            Ok(())
        })
    }
}

/// `tess_sort_merge`: merge sorted runs of items by their key lanes.
///
/// # Safety
///
/// `lanes` must point to `nruns * words` lanes, lane `r * words + w`
/// valid for `left[r]` words; `left` and `more` to `nruns` values;
/// `state` to `TESS_SORT_MERGE_STATE_WORDS` writable words; `out` to
/// `max_out` writable slots; `count` and `refill` writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort_merge(
    nruns: c_int,
    words: c_int,
    lanes: *const *const u64,
    left: *const u32,
    more: *const bool,
    state: *mut u32,
    out: *mut u32,
    max_out: c_int,
    count: *mut c_int,
    refill: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let nruns = usize::try_from(nruns).context("a negative run count")?;
            let words = usize::try_from(words).context("a negative item width")?;
            ensure!(
                nruns <= MAX_MERGE_RUNS,
                "a merge takes up to {MAX_MERGE_RUNS} runs, not {nruns}"
            );
            // Before the lanes are borrowed and counted: their number and
            // the list below come from nruns × words.
            ensure!(
                (1..=MAX_ITEM_WORDS).contains(&words),
                "a sort item has 1 to {MAX_ITEM_WORDS} words, not {words}"
            );
            let left = values(left, nruns, "rows left")?;
            let more = values(more, nruns, "more flags")?;
            let pointers = values(lanes, nruns * words, "lanes")?;
            let mut slices = [&[][..]; MAX_MERGE_RUNS * 2];
            let mut lanes_list: Vec<&[u64]>;
            let lanes: &[&[u64]] = if nruns * words <= slices.len() {
                for (index, &pointer) in pointers.iter().enumerate() {
                    slices[index] = values(pointer, left[index / words] as usize, "lane")?;
                }
                &slices[..nruns * words]
            } else {
                lanes_list = Vec::with_capacity(nruns * words);
                for (index, &pointer) in pointers.iter().enumerate() {
                    lanes_list.push(values(pointer, left[index / words] as usize, "lane")?);
                }
                &lanes_list
            };
            let max_out = usize::try_from(max_out).context("a negative output count")?;
            let out = slots(out, max_out, "merge output")?;
            let state = slots(state, MERGE_STATE_WORDS, "merge state")?;
            let merged = merge(words, lanes, left, more, state, out)?;
            *count.as_mut().context("a null count")? = merged.count as c_int;
            *refill.as_mut().context("a null refill")? =
                merged.refill.map_or(-1, |run| run as c_int);
            Ok(())
        })
    }
}

/// `tess_sort_top_push`: push the items of records into a top-N heap.
///
/// # Safety
///
/// `table` as for the table's `attach` during the call; `keys` as for
/// [`sort_keys`]; `refs` must hold a reference per row of `rows`, a valid
/// mask; `heap` must point to `capacity` items' writable words that
/// nothing else accesses; `len` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_sort_top_push(
    table: *const TableRef,
    nkeys: c_int,
    keys: *const CSortKey,
    refs: *const u32,
    rows: *const Mask,
    heap: *mut u64,
    capacity: usize,
    len: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let mut decoded = [UNUSED; MAX_KEYS];
            let nkeys = sort_keys(nkeys, keys, &mut decoded)?;
            let keys = &decoded[..nkeys];
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let refs = values(refs, rows.nrows(), "references")?;
            let words = capacity
                .checked_mul(item_words(keys)?)
                .context("a heap past the address space")?;
            let heap = slots(heap, words, "heap")?;
            let len = len.as_mut().context("a null heap length")?;
            let mut count =
                usize::try_from(*len).context("a heap length past the address space")?;
            table.top_push(keys, refs, &rows, heap, &mut count)?;
            *len = count as u64;
            Ok(())
        })
    }
}
