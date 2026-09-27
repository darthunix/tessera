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
use tessera_kernels::sort::{SortKey, item_words, sort_items};
use tessera_kernels::table::{KeyKind, MAX_KEYS};

use super::status::{Code, Status, guard};
use super::table::{TableRef, attach, slots};

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
