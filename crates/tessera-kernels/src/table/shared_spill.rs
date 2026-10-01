//! The shared state of a shared table that spills: which partitions the
//! participants split the table into and which of them went to disk,
//! decided once for all of them, and the counters the rounds over the
//! partitions share.
//!
//! The words lie in memory every participant maps, next to the shared
//! table. The table spills when the bytes of its chunks pass the budget:
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
    /// The atomic words a participant sees, by index: those of memory
    /// several processes map, or the loom model's.
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

/// The shared state of a spill over its words.
#[derive(Debug)]
pub struct Spill<W> {
    words: W,
}

/// The shared state in memory several participants map.
pub type SharedSpill<'a> = Spill<&'a [AtomicU64]>;

impl<'a> SharedSpill<'a> {
    /// Attach to the `nwords` words at `words`, as [`words_for`] sized them.
    ///
    /// # Safety
    ///
    /// `words` is aligned to 8 and valid for reads and writes of `nwords`
    /// words for `'a`, which are accessed only through shared spills, here
    /// or in other processes mapping them.
    pub unsafe fn attach(words: *mut u64, nwords: usize) -> Result<Self> {
        ensure!(
            !words.is_null() && words.addr().is_multiple_of(8),
            "a shared spill must be aligned to 8 bytes"
        );
        ensure!(
            nwords > HEAD_WORDS + PART_WORDS && (nwords - HEAD_WORDS).is_multiple_of(PART_WORDS),
            "{nwords} words are no shared spill"
        );
        // SAFETY: the caller's contract; an `AtomicU64` has the size and
        // alignment of a `u64`.
        let words = unsafe { core::slice::from_raw_parts(words.cast::<AtomicU64>(), nwords) };
        Ok(Self { words })
    }
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

    /// Send the partition in memory with the most bytes to disk: its
    /// number for the participant that marked it, `None` for any other or
    /// when none is left in memory.
    pub fn evict_largest(&self) -> Option<u32> {
        let partitions = self.partitions();
        let mut largest = None;
        let mut bytes = 0;
        for partition in 0..partitions {
            if self.words.load(self.part(partition, Field::Flags)) & ON_DISK != 0 {
                continue;
            }
            let held = self.words.load(self.part(partition, Field::Bytes));
            if held > bytes {
                largest = Some(partition);
                bytes = held;
            }
        }
        let partition = largest?;
        let before = self
            .words
            .fetch_or(self.part(partition, Field::Flags), ON_DISK);
        if before & ON_DISK != 0 {
            return None;
        }
        self.words.fetch_add(EVICTIONS, 1);
        Some(partition)
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
    use super::*;

    fn spill(words: &mut Vec<u64>, capacity: usize, budget: u64) -> SharedSpill<'_> {
        *words = vec![0; words_for(capacity).unwrap()];
        // SAFETY: the vector is aligned to 8 and used only here.
        let spill = unsafe { SharedSpill::attach(words.as_mut_ptr(), words.len()) }.unwrap();
        spill.init(budget);
        spill
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
        assert_eq!(spill.evict_largest(), Some(2));
        assert!(spill.on_disk(2).unwrap());
        assert_eq!(spill.evict_largest(), Some(1), "then the next largest");
        assert_eq!(spill.evictions(), 2);
        assert_eq!(spill.evict_largest(), None, "no bytes left in memory");
        assert!(spill.add_bytes(-30, Some(2)).is_ok());
        assert_eq!(spill.bytes(), 80 + 40 + 10);
        assert!(spill.on_disk(4).is_err(), "past the partitions");
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
