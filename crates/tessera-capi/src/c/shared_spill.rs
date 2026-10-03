//! The entry points of a table that spills, declared in
//! `include/tessera/table.h`: the state of its partitions and the rule
//! that sends them to disk (`tessera_kernels::table::shared_spill`), a
//! process's own or the one a shared table's participants decide on, the
//! steps of a round over a partition, and the filling of a shared Bloom
//! filter.

use std::ffi::c_int;

use anyhow::Context;
use tessera_core::RowMaskView;
use tessera_kernels::table::bloom;
use tessera_kernels::table::phases::Participant;
use tessera_kernels::table::shared_spill::{
    LocalSpill, Memory, Records, SharedSpill, Weights, words_for,
};

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

/// A process's own state at `words`, of `nwords` words.
///
/// # Safety
///
/// `words` is aligned to 8 and valid for reads and writes of `nwords`
/// words, which this thread alone accesses, only through local spills,
/// for `'a`.
unsafe fn local<'a>(words: *mut u64, nwords: usize) -> anyhow::Result<LocalSpill<'a>> {
    // SAFETY: the caller's contract.
    unsafe { LocalSpill::attach(words, nwords) }
}

/// `TessSpillWeights`: the weights of the rule that sends partitions to
/// disk, as C passes them.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpillWeights {
    pub start: f64,
    pub target: f64,
    pub spilled: f64,
    pub reserve: u64,
    pub resident: f64,
    pub per_check: u32,
}

impl SpillWeights {
    /// The weights, each a share or a weight of 0 or more.
    fn weights(&self) -> anyhow::Result<Weights> {
        let shares = [self.start, self.target, self.spilled, self.resident];
        anyhow::ensure!(
            shares
                .iter()
                .all(|share| share.is_finite() && *share >= 0.0),
            "spill weights of {shares:?}"
        );
        Ok(Weights {
            start: self.start,
            target: self.target,
            spilled: self.spilled,
            reserve: self.reserve,
            resident: self.resident,
            per_check: self.per_check,
        })
    }
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

/// `tess_table_spill_init`: clear the state, shared or a process's own,
/// for a budget, before any participant uses it.
///
/// # Safety
///
/// As for [`spill`] when `shared`, with no participant using it, and for
/// [`local`] otherwise; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_init(
    words: *mut u64,
    nwords: usize,
    shared: bool,
    budget: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            if shared {
                spill(words, nwords)?.init(budget);
            } else {
                local(words, nwords)?.init(budget);
            }
            Ok(())
        })
    }
}

/// `tess_table_spill_split`: split the table unless another participant
/// did; the partitions in force into `in_force`.
///
/// # Safety
///
/// As for [`tess_table_spill_init`]; `in_force` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_split(
    words: *mut u64,
    nwords: usize,
    shared: bool,
    partitions: u32,
    in_force: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let partitions = if shared {
                spill(words, nwords)?.split(partitions)?
            } else {
                local(words, nwords)?.split(partitions)?
            };
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
/// As for [`tess_table_spill_init`]; `over` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_add_bytes(
    words: *mut u64,
    nwords: usize,
    shared: bool,
    delta: i64,
    partition: i32,
    over: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let partition = partition_of(partition);
            let passed = if shared {
                spill(words, nwords)?.add_bytes(delta, partition)?
            } else {
                local(words, nwords)?.add_bytes(delta, partition)?
            };
            *over.as_mut().context("a null flag")? = passed;
            Ok(())
        })
    }
}

/// `tess_table_spill_evict`: the next partition to send to disk by the
/// weights, marked so, into `partition`, or -1: a shared table's against
/// the bytes its words count and their budget, a process's own against
/// `memory` bytes it measured and `limit`. The records of each partition
/// are the `nrecords` at `records`, of the level `total_records`, or the
/// words' when null; `evicted` counts the partitions the check sent so
/// far.
///
/// # Safety
///
/// As for [`tess_table_spill_init`]; `weights` must point to weights,
/// `records` be null or point to `nrecords` counts, and `partition` be
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_evict(
    words: *mut u64,
    nwords: usize,
    shared: bool,
    weights: *const SpillWeights,
    memory: u64,
    limit: u64,
    records: *const u64,
    nrecords: usize,
    total_records: u64,
    evicted: u32,
    partition: *mut i32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let weights = weights.as_ref().context("null spill weights")?.weights()?;
            let records = (!records.is_null()).then(|| Records {
                each: core::slice::from_raw_parts(records, nrecords),
                total: total_records,
            });
            let marked = if shared {
                spill(words, nwords)?.evict(&weights, Memory::Counted, records, evicted)?
            } else {
                let memory = Memory::Measured {
                    bytes: memory,
                    limit,
                };
                local(words, nwords)?.evict(&weights, memory, records, evicted)?
            };
            *partition.as_mut().context("a null partition")? =
                marked.map_or(-1, |partition| partition as i32);
            Ok(())
        })
    }
}

/// `tess_table_spill_evictions`: the partitions sent to disk so far.
///
/// # Safety
///
/// As for [`spill`]; `evictions` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_spill_evictions(
    words: *mut u64,
    nwords: usize,
    evictions: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *evictions.as_mut().context("a null count")? = spill(words, nwords)?.evictions();
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
