//! The state of a table that spills: which partitions it is split into,
//! the bytes each keeps in memory and which of them went to disk, and the
//! one rule that sends the next to disk ([`Spill::evict`]), whose weights
//! ([`Weights`]) each node gives. A process's own spill keeps the words
//! in its memory ([`LocalSpill`]); a shared table's participants decide
//! once for all of them and share the counters of the rounds over the
//! partitions ([`SharedSpill`]).
//!
//! A shared table's words lie in memory every participant maps, next to
//! the table. The table spills when the bytes of its chunks pass the budget:
//! the first participant to see it sets the partitions, a power of two,
//! with a compare-and-swap, and every other one takes that number. While
//! the chunks still take more than the budget, the largest partition in
//! memory goes to disk, marked by the one participant whose fetch-or set
//! its flag. Each participant then writes its own chunks of that partition;
//! the build's barrier orders every write before the partitions are read.
//!
//! After the build, each partition on disk is a round of its own: its
//! participants take the files of its inner rows, then of its outer rows,
//! one at a time from a counter, and a partition too large for one
//! participant's memory is taken whole by one of them. The loom model in
//! `loom.rs` checks the races.

use core::cell::Cell;
use core::sync::atomic::AtomicU64;

use anyhow::{Result, ensure};

use super::MAX_PARTITIONS;
use super::region::order;

pub(super) use sealed::Words;

/// The flag of a partition that went to disk.
const ON_DISK: u64 = 1;
/// The flag of a partition one participant takes whole.
const ALONE: u64 = 2;

// The words before the partitions': the partitions in force (0 while the
// table is whole), the bytes of chunks in memory of every participant,
// their budget, the counter that spreads the participants over the
// partitions, and the partitions sent to disk so far.
const PARTITIONS: usize = 0;
const BYTES: usize = 1;
const BUDGET: usize = 2;
const START: usize = 3;
const EVICTIONS: usize = 4;
const HEAD_WORDS: usize = 5;

/// A word of a partition, after the head's.
#[derive(Clone, Copy)]
enum Field {
    /// The bytes of its chunks in memory.
    Bytes,
    /// Its records, in memory or on disk.
    Records,
    /// [`ON_DISK`] and [`ALONE`].
    Flags,
    /// The next file of its inner rows to take.
    NextInner,
    /// The next file of its outer rows to take.
    NextOuter,
}

const PART_WORDS: usize = 5;

/// The words of the shared state for up to `partitions` partitions, and
/// one more for the partitions kept in memory, whose outer rows are read
/// back as one.
pub fn words_for(partitions: usize) -> Result<usize> {
    ensure!(
        (1..=MAX_PARTITIONS).contains(&partitions),
        "a shared spill of {partitions} partitions is not 1 to {MAX_PARTITIONS}"
    );
    Ok(HEAD_WORDS + PART_WORDS * (partitions + 1))
}

mod sealed {
    /// The words a spill's state lies in, by index: the atomic words of
    /// memory several processes map or of the loom model, or a process's
    /// own.
    pub trait Words {
        fn load(&self, index: usize) -> u64;
        fn store(&self, index: usize, value: u64);
        fn fetch_add(&self, index: usize, delta: u64) -> u64;
        fn fetch_sub(&self, index: usize, delta: u64) -> u64;
        fn fetch_or(&self, index: usize, bits: u64) -> u64;
        /// Replace `current` by `new`: `Ok` with the value replaced, `Err`
        /// with the value found.
        fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64>;
        /// The most partitions the words hold, besides the resident ones.
        fn capacity(&self) -> usize;
    }
}

impl Words for &[AtomicU64] {
    fn load(&self, index: usize) -> u64 {
        self[index].load(order::LOAD)
    }

    fn store(&self, index: usize, value: u64) {
        self[index].store(value, order::STORE);
    }

    fn fetch_add(&self, index: usize, delta: u64) -> u64 {
        self[index].fetch_add(delta, order::ADD)
    }

    fn fetch_sub(&self, index: usize, delta: u64) -> u64 {
        self[index].fetch_sub(delta, order::ADD)
    }

    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self[index].fetch_or(bits, order::CAS)
    }

    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        self[index].compare_exchange(current, new, order::CAS, order::CAS_FAILED)
    }

    fn capacity(&self) -> usize {
        (self.len() - HEAD_WORDS) / PART_WORDS - 1
    }
}

/// A process's own words, which one thread reads and writes as they are.
impl Words for &[Cell<u64>] {
    fn load(&self, index: usize) -> u64 {
        self[index].get()
    }

    fn store(&self, index: usize, value: u64) {
        self[index].set(value);
    }

    fn fetch_add(&self, index: usize, delta: u64) -> u64 {
        self[index].replace(self[index].get().wrapping_add(delta))
    }

    fn fetch_sub(&self, index: usize, delta: u64) -> u64 {
        self[index].replace(self[index].get().wrapping_sub(delta))
    }

    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self[index].replace(self[index].get() | bits)
    }

    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        let found = self[index].get();
        if found != current {
            return Err(found);
        }
        self[index].set(new);
        Ok(found)
    }

    fn capacity(&self) -> usize {
        (self.len() - HEAD_WORDS) / PART_WORDS - 1
    }
}

/// The state of a spill over its words.
#[derive(Debug)]
pub struct Spill<W> {
    words: W,
}

/// The shared state in memory several participants map.
pub type SharedSpill<'a> = Spill<&'a [AtomicU64]>;

/// The state of a process's own spill, in its memory.
pub type LocalSpill<'a> = Spill<&'a [Cell<u64>]>;

/// Check `nwords` words at `words` for a spill's state, as [`words_for`]
/// sizes them.
fn check_words(words: *mut u64, nwords: usize) -> Result<()> {
    ensure!(
        !words.is_null() && words.addr().is_multiple_of(8),
        "a spill's words must be aligned to 8 bytes"
    );
    ensure!(
        nwords > HEAD_WORDS + PART_WORDS && (nwords - HEAD_WORDS).is_multiple_of(PART_WORDS),
        "{nwords} words are no spill's state"
    );
    Ok(())
}

impl<'a> SharedSpill<'a> {
    /// Attach to the `nwords` words at `words`, as [`words_for`] sized them.
    ///
    /// # Safety
    ///
    /// `words` is aligned to 8 and valid for reads and writes of `nwords`
    /// words for `'a`, which are accessed only through shared spills, here
    /// or in other processes mapping them.
    pub unsafe fn attach(words: *mut u64, nwords: usize) -> Result<Self> {
        check_words(words, nwords)?;
        // SAFETY: the caller's contract; an `AtomicU64` has the size and
        // alignment of a `u64`.
        let words = unsafe { core::slice::from_raw_parts(words.cast::<AtomicU64>(), nwords) };
        Ok(Self { words })
    }
}

impl<'a> LocalSpill<'a> {
    /// Attach to the `nwords` words at `words`, as [`words_for`] sized them.
    ///
    /// # Safety
    ///
    /// `words` is aligned to 8 and valid for reads and writes of `nwords`
    /// words for `'a`, which one thread accesses, only through local
    /// spills.
    pub unsafe fn attach(words: *mut u64, nwords: usize) -> Result<Self> {
        check_words(words, nwords)?;
        // SAFETY: the caller's contract; a `Cell<u64>` has the size and
        // alignment of a `u64`.
        let words = unsafe { core::slice::from_raw_parts(words.cast::<Cell<u64>>(), nwords) };
        Ok(Self { words })
    }
}

/// The weights of the rule that sends partitions to disk
/// ([`Spill::evict`]): each node's rule is a set of their values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weights {
    /// The share of the limit the memory, with the reserve, must pass for
    /// the first partition of a check to go,
    pub start: f64,
    /// and the share it must pass for each one after it.
    pub target: f64,
    /// The weight of a partition's bytes in memory once it went to disk,
    /// in the choice of the largest: 0 leaves it out.
    pub spilled: f64,
    /// The bytes each partition on disk keeps in reserve, counted with the
    /// memory.
    pub reserve: u64,
    /// The share of the records below which the partitions still in memory
    /// all go, once any is on disk: 0 for none.
    pub resident: f64,
    /// The most partitions a check sends, 0 for any.
    pub per_check: u32,
}

/// The records of a process's own spill: each partition's, and those of
/// the whole level, which a level split from a partition knows before its
/// partitions have them all.
#[derive(Clone, Copy, Debug)]
pub struct Records<'a> {
    pub each: &'a [u64],
    pub total: u64,
}

/// What a check weighs against its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Memory {
    /// The bytes the words count, against their budget: a shared table's,
    /// every participant's chunks.
    Counted,
    /// The bytes a process measures, against its limit; a limit of
    /// `u64::MAX` leaves the rule of the records alone.
    Measured { bytes: u64, limit: u64 },
}

impl<W: Words> Spill<W> {
    /// The state over `words`, which [`Spill::init`] clears.
    #[cfg(all(test, loom))]
    pub(super) fn over(words: W) -> Self {
        Self { words }
    }

    /// Clear the state for a budget of bytes, before any participant uses it.
    pub fn init(&self, budget: u64) {
        for index in 0..HEAD_WORDS + PART_WORDS * (self.words.capacity() + 1) {
            self.words.store(index, 0);
        }
        self.words.store(BUDGET, budget);
    }

    /// The partitions, or 0 while the table is whole.
    pub fn partitions(&self) -> u32 {
        self.words.load(PARTITIONS) as u32
    }

    /// Split the table into `partitions`, a power of two, unless another
    /// participant did: the partitions in force are returned.
    pub fn split(&self, partitions: u32) -> Result<u32> {
        ensure!(
            partitions.is_power_of_two() && partitions as usize <= self.words.capacity(),
            "{partitions} partitions are not a power of two up to {}",
            self.words.capacity()
        );
        Ok(
            match self
                .words
                .compare_exchange(PARTITIONS, 0, u64::from(partitions))
            {
                Ok(_) => partitions,
                Err(current) => current as u32,
            },
        )
    }

    /// Add `delta` bytes of chunks in memory, of `partition` once the
    /// table is split; true when the chunks take more than the budget.
    pub fn add_bytes(&self, delta: i64, partition: Option<u32>) -> Result<bool> {
        if let Some(partition) = partition {
            self.check_partition(partition)?;
            self.add_signed(self.part(partition, Field::Bytes), delta)?;
        }
        let total = self.add_signed(BYTES, delta)?;
        Ok(total > self.words.load(BUDGET))
    }

    /// The bytes of chunks in memory, of every participant.
    pub fn bytes(&self) -> u64 {
        self.words.load(BYTES)
    }

    /// The next partition to send to disk, marked so, or `None`; `evicted`
    /// counts those the check sent so far. While the memory and the
    /// reserve of the partitions on disk pass `start` of the limit at a
    /// check's first and `target` after, the partition with the most bytes
    /// in memory goes, those on disk weighed by `spilled`; then, with any
    /// on disk and those in memory holding fewer than `resident` of the
    /// records, each of these. The records are the caller's, or the
    /// words' when `records` is `None`. A partition
    /// another participant marked since it was chosen gives `None`, as
    /// does a check past `per_check`.
    pub fn evict(
        &self,
        weights: &Weights,
        memory: Memory,
        records: Option<Records<'_>>,
        evicted: u32,
    ) -> Result<Option<u32>> {
        let partitions = self.partitions();
        if let Some(records) = records {
            ensure!(
                records.each.len() == partitions as usize,
                "{} counts of records for {partitions} partitions",
                records.each.len()
            );
        }
        if weights.per_check > 0 && evicted >= weights.per_check {
            return Ok(None);
        }
        let (bytes, limit) = match memory {
            Memory::Counted => (self.bytes(), self.words.load(BUDGET)),
            Memory::Measured { bytes, limit } => (bytes, limit),
        };
        let mut largest = None;
        let mut most = 0.0;
        let mut on_disk = 0_u64;
        let (mut resident, mut total) = (0_u64, 0_u64);
        let mut first_resident = None;
        for partition in 0..partitions {
            let spilled = self.words.load(self.part(partition, Field::Flags)) & ON_DISK != 0;
            let weight = if spilled { weights.spilled } else { 1.0 };
            let held = self.words.load(self.part(partition, Field::Bytes)) as f64 * weight;
            if held > most {
                largest = Some((partition, spilled));
                most = held;
            }
            let count = match records {
                Some(records) => records.each[partition as usize],
                None => self.words.load(self.part(partition, Field::Records)),
            };
            total += count;
            if spilled {
                on_disk += 1;
            } else {
                resident += count;
                first_resident.get_or_insert(partition);
            }
        }
        let share = if evicted == 0 {
            weights.start
        } else {
            weights.target
        };
        if let Some(records) = records {
            total = records.total;
        }
        let held = bytes as f64 + weights.reserve as f64 * on_disk as f64;
        if held > share * limit as f64
            && let Some((partition, spilled)) = largest
        {
            return Ok(self.mark(partition, spilled));
        }
        if on_disk > 0 && resident > 0 && (resident as f64) < weights.resident * total as f64 {
            return Ok(first_resident.and_then(|partition| self.mark(partition, false)));
        }
        Ok(None)
    }

    /// Mark a partition on disk, chosen while it was there or not: its
    /// number, or `None` when another participant marked it since.
    fn mark(&self, partition: u32, spilled: bool) -> Option<u32> {
        let before = self
            .words
            .fetch_or(self.part(partition, Field::Flags), ON_DISK);
        if before & ON_DISK == 0 {
            self.words.fetch_add(EVICTIONS, 1);
            return Some(partition);
        }
        spilled.then_some(partition)
    }

    /// The partitions sent to disk so far: a participant that saw fewer
    /// writes its chunks of the new ones.
    pub fn evictions(&self) -> u64 {
        self.words.load(EVICTIONS)
    }

    /// Whether the partition went to disk.
    pub fn on_disk(&self, partition: u32) -> Result<bool> {
        self.check_partition(partition)?;
        Ok(self.words.load(self.part(partition, Field::Flags)) & ON_DISK != 0)
    }

    /// Count records of a partition, in memory or on disk.
    pub fn add_records(&self, partition: u32, records: u64) -> Result<()> {
        self.check_partition(partition)?;
        self.words
            .fetch_add(self.part(partition, Field::Records), records);
        Ok(())
    }

    /// The records of a partition, once the build is over.
    pub fn records(&self, partition: u32) -> Result<u64> {
        self.check_partition(partition)?;
        Ok(self.words.load(self.part(partition, Field::Records)))
    }

    /// The partition a participant starts its rounds at, spread over them.
    pub fn start(&self) -> u32 {
        let partitions = u64::from(self.partitions().max(1));
        (self.words.fetch_add(START, 1) % partitions) as u32
    }

    /// Take the next file of a partition's inner rows, or of its outer
    /// rows: each number goes to one participant; `partition` may be the
    /// partitions' count, for the outer rows of those kept in memory.
    pub fn take_file(&self, partition: u32, outer: bool) -> Result<u32> {
        ensure!(
            partition <= self.partitions(),
            "partition {partition} is past the {} of the table",
            self.partitions()
        );
        let field = if outer {
            Field::NextOuter
        } else {
            Field::NextInner
        };
        Ok(self.words.fetch_add(self.part(partition, field), 1) as u32)
    }

    /// Take a partition whole: true for the one participant that did.
    pub fn take_alone(&self, partition: u32) -> Result<bool> {
        self.check_partition(partition)?;
        Ok(self
            .words
            .fetch_or(self.part(partition, Field::Flags), ALONE)
            & ALONE
            == 0)
    }

    /// Whether one participant took the partition whole.
    pub fn alone(&self, partition: u32) -> Result<bool> {
        self.check_partition(partition)?;
        Ok(self.words.load(self.part(partition, Field::Flags)) & ALONE != 0)
    }

    /// The index of a partition's word.
    fn part(&self, partition: u32, field: Field) -> usize {
        HEAD_WORDS + PART_WORDS * partition as usize + field as usize
    }

    /// Add a signed delta to a counter; the new value. Callers take away
    /// only what they added, so a counter below zero is an error of the
    /// accounting, reported rather than wrapped; the counter is then left
    /// wrapped, and the query fails with it.
    fn add_signed(&self, index: usize, delta: i64) -> Result<u64> {
        let amount = delta.unsigned_abs();
        if delta >= 0 {
            return Ok(self.words.fetch_add(index, amount) + amount);
        }
        let before = self.words.fetch_sub(index, amount);
        ensure!(
            before >= amount,
            "a spill counter went below zero: {before} bytes less {amount}"
        );
        Ok(before - amount)
    }

    fn check_partition(&self, partition: u32) -> Result<()> {
        ensure!(
            partition < self.partitions(),
            "partition {partition} is past the {} of the table",
            self.partitions()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// A shared table's weights: past its budget, one partition a check.
    const SHARED: Weights = Weights {
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        reserve: 0,
        resident: 0.0,
        per_check: 1,
    };

    /// A grouping's: from seven eighths of the limit down to half, the
    /// largest of every partition.
    const GROUPING: Weights = Weights {
        start: 0.875,
        target: 0.5,
        spilled: 1.0,
        reserve: 0,
        resident: 0.0,
        per_check: 0,
    };

    /// A join's: tails of 25 bytes in reserve, the partitions in memory
    /// only, and all of them under a quarter of the records.
    const JOIN: Weights = Weights {
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        reserve: 25,
        resident: 0.25,
        per_check: 0,
    };

    fn spill(words: &mut Vec<u64>, capacity: usize, budget: u64) -> SharedSpill<'_> {
        *words = vec![0; words_for(capacity).unwrap()];
        // SAFETY: the vector is aligned to 8 and used only here.
        let spill = unsafe { SharedSpill::attach(words.as_mut_ptr(), words.len()) }.unwrap();
        spill.init(budget);
        spill
    }

    /// A process's own state of four partitions with the bytes given.
    fn local(words: &mut Vec<u64>, bytes: [i64; 4]) -> LocalSpill<'_> {
        *words = vec![0; words_for(4).unwrap()];
        // SAFETY: the vector is aligned to 8 and used only here.
        let spill = unsafe { LocalSpill::attach(words.as_mut_ptr(), words.len()) }.unwrap();
        spill.init(0);
        spill.split(4).unwrap();
        for (partition, bytes) in (0..).zip(bytes) {
            spill.add_bytes(bytes, Some(partition)).unwrap();
        }
        spill
    }

    fn measured(bytes: u64, limit: u64) -> Memory {
        Memory::Measured { bytes, limit }
    }

    #[test]
    fn the_first_split_holds_and_the_largest_partition_goes_to_disk() {
        let mut words = Vec::new();
        let spill = spill(&mut words, 16, 100);
        assert_eq!(spill.partitions(), 0);
        assert!(!spill.add_bytes(80, None).unwrap());
        assert!(spill.add_bytes(40, None).unwrap());
        assert!(spill.split(3).is_err(), "not a power of two");
        assert_eq!(spill.split(4).unwrap(), 4);
        assert_eq!(spill.split(8).unwrap(), 4, "the first split holds");
        spill.add_bytes(10, Some(1)).unwrap();
        spill.add_bytes(30, Some(2)).unwrap();
        let evict = |evicted| {
            spill
                .evict(&SHARED, Memory::Counted, None, evicted)
                .unwrap()
        };
        assert_eq!(evict(1), None, "one a check");
        assert_eq!(evict(0), Some(2));
        assert!(spill.on_disk(2).unwrap());
        assert_eq!(evict(0), Some(1), "then the next largest");
        assert_eq!(spill.evictions(), 2);
        assert_eq!(evict(0), None, "no bytes left in memory");
        assert!(spill.add_bytes(-30, Some(2)).is_ok());
        assert_eq!(spill.bytes(), 80 + 40 + 10);
        assert!(spill.on_disk(4).is_err(), "past the partitions");
    }

    #[test]
    fn a_grouping_goes_from_its_start_down_to_its_target() {
        let mut words = Vec::new();
        let spill = local(&mut words, [40, 30, 20, 10]);
        let evict = |weights, bytes, evicted| {
            spill
                .evict(weights, measured(bytes, 100), None, evicted)
                .unwrap()
        };
        assert_eq!(evict(&GROUPING, 87, 0), None, "below seven eighths");
        assert_eq!(evict(&GROUPING, 88, 0), Some(0));
        spill.add_bytes(-40, Some(0)).unwrap();
        assert_eq!(evict(&GROUPING, 50, 1), None, "down to half");
        assert_eq!(evict(&GROUPING, 51, 1), Some(1));
        spill.add_bytes(-30, Some(1)).unwrap();
        spill.add_bytes(50, Some(0)).unwrap();
        assert_eq!(
            evict(&GROUPING, 90, 0),
            Some(0),
            "on disk, and largest again"
        );
        assert_eq!(spill.evictions(), 2, "counted once");
        let left_out = Weights {
            spilled: 0.0,
            ..GROUPING
        };
        assert_eq!(evict(&left_out, 90, 0), Some(2), "on disk, left out");
    }

    #[test]
    fn a_join_keeps_reserve_for_its_tails_and_a_quarter_in_memory() {
        let mut words = Vec::new();
        let spill = local(&mut words, [30, 20, 10, 0]);
        let each = [60, 20, 12, 8];
        let records = Records {
            each: &each,
            total: 100,
        };
        let evict = |bytes, limit| {
            spill
                .evict(&JOIN, measured(bytes, limit), Some(records), 0)
                .unwrap()
        };
        assert_eq!(evict(100, 100), None, "at the limit");
        assert_eq!(evict(101, 100), Some(0));
        assert_eq!(evict(75, 100), None, "75 and a tail of 25");
        assert_eq!(evict(76, 100), Some(1), "the largest in memory");
        // 12 + 8 records left in memory, under a quarter of 100.
        assert_eq!(evict(0, u64::MAX), Some(2), "the records' rule alone");
        assert_eq!(evict(0, 100), Some(3), "without bytes too");
        assert_eq!(evict(0, 100), None, "none left in memory");
        let short = Records {
            each: &each[..3],
            ..records
        };
        assert!(
            spill
                .evict(&JOIN, measured(0, 100), Some(short), 0)
                .is_err(),
            "a count for each partition"
        );
    }

    #[test]
    fn a_level_being_split_weighs_the_records_it_will_hold() {
        let mut words = Vec::new();
        let spill = local(&mut words, [20, 10, 0, 0]);
        let each = [10, 10, 0, 0];
        let evict = |total| {
            let records = Records { each: &each, total };
            spill
                .evict(&JOIN, measured(0, u64::MAX), Some(records), 0)
                .unwrap()
        };
        assert_eq!(
            spill.evict(&JOIN, measured(101, 100), None, 0).unwrap(),
            Some(0)
        );
        assert_eq!(evict(20), None, "half of the rows split so far");
        assert_eq!(evict(100), Some(1), "a tenth of the level's");
    }

    #[test]
    fn the_records_rule_holds_at_its_edges() {
        let mut words = Vec::new();
        let spill = local(&mut words, [0, 0, 0, 0]);
        let rule = |each: [u64; 4], total| {
            let records = Records { each: &each, total };
            spill
                .evict(&JOIN, measured(0, u64::MAX), Some(records), 0)
                .unwrap()
        };
        assert_eq!(rule([20, 0, 0, 0], 100), None, "nothing on disk");
        assert_eq!(
            spill.evict(&JOIN, measured(101, 100), None, 0).unwrap(),
            None,
            "no bytes"
        );
        let spill = local(&mut words, [10, 0, 0, 0]);
        assert_eq!(
            spill.evict(&JOIN, measured(101, 100), None, 0).unwrap(),
            Some(0)
        );
        let rule = |each: [u64; 4], total| {
            let records = Records { each: &each, total };
            spill
                .evict(&JOIN, measured(0, u64::MAX), Some(records), 0)
                .unwrap()
        };
        assert_eq!(rule([100, 0, 0, 0], 100), None, "no record in memory");
        assert_eq!(rule([75, 25, 0, 0], 100), None, "a quarter exactly");
        assert_eq!(rule([76, 24, 0, 0], 100), Some(1), "under a quarter");
        // The words' own records, as a shared table counts them.
        spill.add_records(0, 80).unwrap();
        spill.add_records(2, 10).unwrap();
        spill.add_records(3, 10).unwrap();
        assert_eq!(
            spill.evict(&JOIN, measured(0, u64::MAX), None, 0).unwrap(),
            Some(2),
            "20 of 100 of the words' records"
        );
    }

    #[test]
    fn words_attach_only_as_words_for_sizes_them() {
        let mut words = vec![0_u64; words_for(4).unwrap() + 1];
        let at = words.as_mut_ptr();
        let len = words_for(4).unwrap();
        // SAFETY: the vector is aligned to 8 and used only here; the
        // calls that succeed read nothing.
        unsafe {
            assert!(SharedSpill::attach(at, len).is_ok());
            assert!(LocalSpill::attach(at, len).is_ok());
            assert!(
                LocalSpill::attach(at, len + 1).is_err(),
                "not a whole partition"
            );
            assert!(
                SharedSpill::attach(at, HEAD_WORDS + PART_WORDS).is_err(),
                "no partition"
            );
            assert!(
                LocalSpill::attach(core::ptr::null_mut(), len).is_err(),
                "null"
            );
            let odd = at.cast::<u8>().add(4).cast::<u64>();
            assert!(SharedSpill::attach(odd, len).is_err(), "not aligned to 8");
        }
    }

    proptest! {
        /// A process's own words take the same steps as the shared ones.
        #[test]
        fn local_words_choose_as_shared_ones_do(
            adds in proptest::collection::vec((0_u32..8, 0_i64..1000, 0_u64..100), 0..32),
            checks in proptest::collection::vec((0_u64..4000, 0_u32..3, 0_usize..3), 0..16),
        ) {
            let mut shared_words = vec![0; words_for(8).unwrap()];
            let mut local_words = shared_words.clone();
            {
                let (at, len) = (shared_words.as_mut_ptr(), shared_words.len());
                // SAFETY: the vectors are aligned to 8 and used only here.
                let shared = unsafe { SharedSpill::attach(at, len) }.unwrap();
                let (at, len) = (local_words.as_mut_ptr(), local_words.len());
                // SAFETY: as above.
                let local = unsafe { LocalSpill::attach(at, len) }.unwrap();
                shared.init(2000);
                local.init(2000);
                prop_assert_eq!(shared.split(8).unwrap(), local.split(8).unwrap());
                for &(partition, bytes, records) in &adds {
                    prop_assert_eq!(
                        shared.add_bytes(bytes, Some(partition)).unwrap(),
                        local.add_bytes(bytes, Some(partition)).unwrap()
                    );
                    shared.add_records(partition, records).unwrap();
                    local.add_records(partition, records).unwrap();
                }
                for &(bytes, evicted, weights) in &checks {
                    let weights = [SHARED, GROUPING, JOIN][weights];
                    for memory in [Memory::Counted, measured(bytes, 2000)] {
                        prop_assert_eq!(
                            shared.evict(&weights, memory, None, evicted).unwrap(),
                            local.evict(&weights, memory, None, evicted).unwrap()
                        );
                    }
                }
            }
            prop_assert_eq!(shared_words, local_words);
        }
    }

    #[test]
    fn files_and_partitions_are_taken_once() {
        let mut words = Vec::new();
        let spill = spill(&mut words, 4, 0);
        spill.split(4).unwrap();
        assert_eq!(spill.take_file(1, false).unwrap(), 0);
        assert_eq!(spill.take_file(1, false).unwrap(), 1);
        assert_eq!(spill.take_file(1, true).unwrap(), 0);
        assert_eq!(spill.take_file(4, true).unwrap(), 0, "the resident rows");
        assert!(spill.take_file(5, true).is_err());
        assert!(spill.take_alone(3).unwrap());
        assert!(!spill.take_alone(3).unwrap());
        assert!(spill.alone(3).unwrap());
        spill.add_records(0, 7).unwrap();
        assert_eq!(spill.records(0).unwrap(), 7);
        let starts: Vec<u32> = (0..5).map(|_| spill.start()).collect();
        assert_eq!(starts, [0, 1, 2, 3, 0]);
    }

    #[test]
    fn a_counter_never_goes_below_zero() {
        let mut words = Vec::new();
        let spill = spill(&mut words, 4, 100);
        spill.add_bytes(10, None).unwrap();
        assert!(spill.add_bytes(-10, None).is_ok(), "down to zero");
        assert_eq!(spill.bytes(), 0);
        spill.add_bytes(10, None).unwrap();
        let error = spill.add_bytes(-20, None).unwrap_err();
        assert!(error.to_string().contains("below zero"), "{error}");
        let spill = self::spill(&mut words, 4, 100);
        spill.split(4).unwrap();
        spill.add_bytes(50, Some(0)).unwrap();
        spill.add_bytes(10, Some(1)).unwrap();
        assert!(
            spill.add_bytes(-20, Some(1)).is_err(),
            "a partition's bytes, while the table's total still covers them"
        );
    }
}
