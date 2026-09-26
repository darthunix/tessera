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

/// The flag of a partition that went to disk.
const ON_DISK: u64 = 1;
/// The flag of a partition one participant takes whole.
const ALONE: u64 = 2;

/// Words before the partitions': the partitions, the bytes in memory, the
/// budget, the counter that spreads the participants over the partitions.
const HEAD_WORDS: usize = 4;
/// Words per partition: bytes in memory, records, flags, the next inner
/// file and the next outer file to take.
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

/// The atomic words a participant sees, by index; [`SharedSpill`] and the
/// loom model share the logic over them.
pub(super) trait Words {
    fn load(&self, index: usize) -> u64;
    fn store(&self, index: usize, value: u64);
    fn fetch_add(&self, index: usize, delta: u64) -> u64;
    fn fetch_sub(&self, index: usize, delta: u64) -> u64;
    fn fetch_or(&self, index: usize, bits: u64) -> u64;
    /// Replace `current` by `new`: `Ok` with the value replaced, `Err` with
    /// the value found.
    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64>;
    /// The most partitions the words hold, besides the resident ones.
    fn capacity(&self) -> usize;
}

/// The shared state in memory several participants map.
#[derive(Debug)]
pub struct SharedSpill<'a> {
    words: &'a [AtomicU64],
}

impl Words for SharedSpill<'_> {
    fn load(&self, index: usize) -> u64 {
        self.words[index].load(order::LOAD)
    }

    fn store(&self, index: usize, value: u64) {
        self.words[index].store(value, order::STORE);
    }

    fn fetch_add(&self, index: usize, delta: u64) -> u64 {
        self.words[index].fetch_add(delta, order::ADD)
    }

    fn fetch_sub(&self, index: usize, delta: u64) -> u64 {
        self.words[index].fetch_sub(delta, order::ADD)
    }

    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self.words[index].fetch_or(bits, order::CAS)
    }

    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        self.words[index].compare_exchange(current, new, order::CAS, order::CAS_FAILED)
    }

    fn capacity(&self) -> usize {
        (self.words.len() - HEAD_WORDS) / PART_WORDS - 1
    }
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

/// The logic, over any [`Words`].
pub(super) trait Spill: Words {
    /// Clear the state for a budget of bytes, before any participant uses it.
    fn init(&self, budget: u64) {
        for index in 0..HEAD_WORDS + PART_WORDS * (self.capacity() + 1) {
            self.store(index, 0);
        }
        self.store(2, budget);
    }

    /// The partitions, or 0 while the table is whole.
    fn partitions(&self) -> u32 {
        self.load(0) as u32
    }

    /// Split the table into `partitions`, a power of two, unless another
    /// participant did: the partitions in force are returned.
    fn split(&self, partitions: u32) -> Result<u32> {
        ensure!(
            partitions.is_power_of_two() && partitions as usize <= self.capacity(),
            "{partitions} partitions are not a power of two up to {}",
            self.capacity()
        );
        Ok(match self.compare_exchange(0, 0, u64::from(partitions)) {
            Ok(_) => partitions,
            Err(current) => current as u32,
        })
    }

    /// Add `delta` bytes of chunks in memory, of `partition` once the
    /// table is split; true when the chunks take more than the budget.
    fn add_bytes(&self, delta: i64, partition: Option<u32>) -> Result<bool> {
        if let Some(partition) = partition {
            self.check_partition(partition)?;
            self.add_signed(self.part(partition, 0), delta);
        }
        let total = self.add_signed(1, delta);
        Ok(total > self.load(2))
    }

    /// The bytes of chunks in memory, of every participant.
    fn bytes(&self) -> u64 {
        self.load(1)
    }

    /// Send the partition in memory with the most bytes to disk: its
    /// number for the participant that marked it, `None` for any other or
    /// when none is left in memory.
    fn evict_largest(&self) -> Option<u32> {
        let partitions = self.partitions();
        let mut largest = None;
        let mut bytes = 0;
        for partition in 0..partitions {
            if self.load(self.part(partition, 2)) & ON_DISK != 0 {
                continue;
            }
            let held = self.load(self.part(partition, 0));
            if held > bytes {
                largest = Some(partition);
                bytes = held;
            }
        }
        let partition = largest?;
        let before = self.fetch_or(self.part(partition, 2), ON_DISK);
        (before & ON_DISK == 0).then_some(partition)
    }

    /// Whether the partition went to disk.
    fn on_disk(&self, partition: u32) -> Result<bool> {
        self.check_partition(partition)?;
        Ok(self.load(self.part(partition, 2)) & ON_DISK != 0)
    }

    /// Count records of a partition, in memory or on disk.
    fn add_records(&self, partition: u32, records: u64) -> Result<()> {
        self.check_partition(partition)?;
        self.fetch_add(self.part(partition, 1), records);
        Ok(())
    }

    /// The records of a partition, once the build is over.
    fn records(&self, partition: u32) -> Result<u64> {
        self.check_partition(partition)?;
        Ok(self.load(self.part(partition, 1)))
    }

    /// The partition a participant starts its rounds at, spread over them.
    fn start(&self) -> u32 {
        let partitions = u64::from(self.partitions().max(1));
        (self.fetch_add(3, 1) % partitions) as u32
    }

    /// Take the next file of a partition's inner rows, or of its outer
    /// rows: each number goes to one participant; `partition` may be the
    /// partitions' count, for the outer rows of those kept in memory.
    fn take_file(&self, partition: u32, outer: bool) -> Result<u32> {
        ensure!(
            partition <= self.partitions(),
            "partition {partition} is past the {} of the table",
            self.partitions()
        );
        Ok(self.fetch_add(self.part(partition, if outer { 4 } else { 3 }), 1) as u32)
    }

    /// Take a partition whole: true for the one participant that did.
    fn take_alone(&self, partition: u32) -> Result<bool> {
        self.check_partition(partition)?;
        Ok(self.fetch_or(self.part(partition, 2), ALONE) & ALONE == 0)
    }

    /// Whether one participant took the partition whole.
    fn alone(&self, partition: u32) -> Result<bool> {
        self.check_partition(partition)?;
        Ok(self.load(self.part(partition, 2)) & ALONE != 0)
    }

    /// The index of a partition's field.
    fn part(&self, partition: u32, field: usize) -> usize {
        HEAD_WORDS + PART_WORDS * partition as usize + field
    }

    /// Add a signed delta to a counter; the new value.
    fn add_signed(&self, index: usize, delta: i64) -> u64 {
        if delta >= 0 {
            self.fetch_add(index, delta as u64) + delta as u64
        } else {
            self.fetch_sub(index, delta.unsigned_abs()) - delta.unsigned_abs()
        }
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

impl<W: Words + ?Sized> Spill for W {}

impl SharedSpill<'_> {
    /// See [`Spill::init`].
    pub fn init(&self, budget: u64) {
        Spill::init(self, budget);
    }
    /// See [`Spill::partitions`].
    pub fn partitions(&self) -> u32 {
        Spill::partitions(self)
    }
    /// See [`Spill::split`].
    pub fn split(&self, partitions: u32) -> Result<u32> {
        Spill::split(self, partitions)
    }
    /// See [`Spill::add_bytes`].
    pub fn add_bytes(&self, delta: i64, partition: Option<u32>) -> Result<bool> {
        Spill::add_bytes(self, delta, partition)
    }
    /// See [`Spill::bytes`].
    pub fn bytes(&self) -> u64 {
        Spill::bytes(self)
    }
    /// See [`Spill::evict_largest`].
    pub fn evict_largest(&self) -> Option<u32> {
        Spill::evict_largest(self)
    }
    /// See [`Spill::on_disk`].
    pub fn on_disk(&self, partition: u32) -> Result<bool> {
        Spill::on_disk(self, partition)
    }
    /// See [`Spill::add_records`].
    pub fn add_records(&self, partition: u32, records: u64) -> Result<()> {
        Spill::add_records(self, partition, records)
    }
    /// See [`Spill::records`].
    pub fn records(&self, partition: u32) -> Result<u64> {
        Spill::records(self, partition)
    }
    /// See [`Spill::start`].
    pub fn start(&self) -> u32 {
        Spill::start(self)
    }
    /// See [`Spill::take_file`].
    pub fn take_file(&self, partition: u32, outer: bool) -> Result<u32> {
        Spill::take_file(self, partition, outer)
    }
    /// See [`Spill::take_alone`].
    pub fn take_alone(&self, partition: u32) -> Result<bool> {
        Spill::take_alone(self, partition)
    }
    /// See [`Spill::alone`].
    pub fn alone(&self, partition: u32) -> Result<bool> {
        Spill::alone(self, partition)
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
}
