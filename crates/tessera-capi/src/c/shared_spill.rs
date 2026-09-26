//! The entry points of a shared table that spills, declared in
//! `include/tessera/table.h`: the shared state its participants decide on
//! (`tessera_kernels::table::shared_spill`), the steps of a round over a
//! partition, and the filling of a shared Bloom filter.

use std::ffi::c_int;

use anyhow::Context;
use tessera_core::RowMaskView;
use tessera_kernels::table::bloom;
use tessera_kernels::table::phases::Participant;
use tessera_kernels::table::shared_spill::{SharedSpill, words_for};

use super::mask::Mask;
use super::status::{Code, Status, guard};

/// The shared state at `words`, of `nwords` words.
///
/// # Safety
///
/// `words` is aligned to 8 and valid for reads and writes of `nwords`
/// words, accessed only through shared spills, for `'a`.
unsafe fn spill<'a>(words: *mut u64, nwords: usize) -> anyhow::Result<SharedSpill<'a>> {
    // SAFETY: the caller's contract.
    unsafe { SharedSpill::attach(words, nwords) }
}

/// A partition number from C, where -1 stands for none.
fn partition_of(partition: i32) -> Option<u32> {
    u32::try_from(partition).ok()
}

/// `tess_table_spill_words`: the words of the shared state for up to
/// `capacity` partitions.
///
/// # Safety
///
/// `nwords` must be writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_words(
    capacity: c_int,
    nwords: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let capacity = usize::try_from(capacity).context("a negative capacity")?;
            *nwords.as_mut().context("a null word count")? = words_for(capacity)?;
            Ok(())
        })
    }
}

/// `tess_table_spill_init`: clear the shared state for a budget, before
/// any participant uses it.
///
/// # Safety
///
/// As for [`spill`], with no participant using it; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_init(
    words: *mut u64,
    nwords: usize,
    budget: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            spill(words, nwords)?.init(budget);
            Ok(())
        })
    }
}

/// `tess_table_spill_split`: split the table unless another participant
/// did; the partitions in force into `in_force`.
///
/// # Safety
///
/// As for [`spill`]; `in_force` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_split(
    words: *mut u64,
    nwords: usize,
    partitions: u32,
    in_force: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let partitions = spill(words, nwords)?.split(partitions)?;
            *in_force.as_mut().context("a null partition count")? = partitions;
            Ok(())
        })
    }
}

/// `tess_table_spill_partitions`: the partitions, 0 while whole.
///
/// # Safety
///
/// As for [`spill`]; `partitions` must be writable; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_partitions(
    words: *mut u64,
    nwords: usize,
    partitions: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *partitions.as_mut().context("a null partition count")? =
                spill(words, nwords)?.partitions();
            Ok(())
        })
    }
}

/// `tess_table_spill_add_bytes`: add bytes of chunks in memory, of a
/// partition or of none (-1); whether they pass the budget into `over`.
///
/// # Safety
///
/// As for [`spill`]; `over` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_add_bytes(
    words: *mut u64,
    nwords: usize,
    delta: i64,
    partition: i32,
    over: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let passed = spill(words, nwords)?.add_bytes(delta, partition_of(partition))?;
            *over.as_mut().context("a null flag")? = passed;
            Ok(())
        })
    }
}

/// `tess_table_spill_evict`: send the largest partition in memory to
/// disk; its number into `partition` for the participant that marked it,
/// -1 otherwise.
///
/// # Safety
///
/// As for [`spill`]; `partition` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_evict(
    words: *mut u64,
    nwords: usize,
    partition: *mut i32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let marked = spill(words, nwords)?.evict_largest();
            *partition.as_mut().context("a null partition")? =
                marked.map_or(-1, |partition| partition as i32);
            Ok(())
        })
    }
}

/// `tess_table_spill_flags`: whether a partition went to disk, and
/// whether one participant took it whole.
///
/// # Safety
///
/// As for [`spill`]; `on_disk` and `alone` must be writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_flags(
    words: *mut u64,
    nwords: usize,
    partition: u32,
    on_disk: *mut bool,
    alone: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let spill = spill(words, nwords)?;
            *on_disk.as_mut().context("a null flag")? = spill.on_disk(partition)?;
            *alone.as_mut().context("a null flag")? = spill.alone(partition)?;
            Ok(())
        })
    }
}

/// `tess_table_spill_records`: add records to a partition, then its
/// records into `records` unless null.
///
/// # Safety
///
/// As for [`spill`]; `records` null or writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_records(
    words: *mut u64,
    nwords: usize,
    partition: u32,
    added: u64,
    records: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let spill = spill(words, nwords)?;
            if added > 0 {
                spill.add_records(partition, added)?;
            }
            if let Some(records) = records.as_mut() {
                *records = spill.records(partition)?;
            }
            Ok(())
        })
    }
}

/// `tess_table_spill_start`: the partition a participant starts its
/// rounds at.
///
/// # Safety
///
/// As for [`spill`]; `partition` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_start(
    words: *mut u64,
    nwords: usize,
    partition: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *partition.as_mut().context("a null partition")? = spill(words, nwords)?.start();
            Ok(())
        })
    }
}

/// `tess_table_spill_take_file`: the next file of a partition's inner or
/// outer rows to read, the partitions' count standing for the resident
/// ones' outer rows.
///
/// # Safety
///
/// As for [`spill`]; `file` must be writable; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_take_file(
    words: *mut u64,
    nwords: usize,
    partition: u32,
    outer: bool,
    file: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *file.as_mut().context("a null file")? =
                spill(words, nwords)?.take_file(partition, outer)?;
            Ok(())
        })
    }
}

/// `tess_table_spill_take_alone`: take a partition whole; whether this
/// participant did into `taken`.
///
/// # Safety
///
/// As for [`spill`]; `taken` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_take_alone(
    words: *mut u64,
    nwords: usize,
    partition: u32,
    taken: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *taken.as_mut().context("a null flag")? =
                spill(words, nwords)?.take_alone(partition)?;
            Ok(())
        })
    }
}

/// `tess_round_step`: a round participant's next action, given what its
/// barrier operation returned.
///
/// # Safety
///
/// `participant` must point to a participant this process alone uses,
/// zeroed before its first step; `action` must be writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_round_step(
    participant: *mut Participant,
    reply: u32,
    action: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let participant = participant.as_mut().context("a null participant")?;
            *action.as_mut().context("a null action")? = participant.round_step(reply)? as u32;
            Ok(())
        })
    }
}

/// `tess_bloom_shared_add`: set the bits of a batch's hashes in a filter
/// several participants fill at once.
///
/// # Safety
///
/// `words` as for [`bloom::add_shared`]; `rows` a valid mask; `hashes` a
/// hash per row; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_shared_add(
    words: *mut u64,
    nwords: usize,
    hashes: *const u32,
    rows: *const Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows: RowMaskView<'_> = rows.as_ref().context("a null row mask")?.view()?;
            let hashes = if rows.nrows() == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(hashes.as_ref().context("null hashes")?, rows.nrows())
            };
            bloom::add_shared(words, nwords, hashes, &rows)
        })
    }
}
