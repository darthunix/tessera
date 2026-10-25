// The crate's page is its README.
#![doc = include_str!("../README.md")]
// Unsafe code lives only in the backends, which call the system; every
// block there states its invariants.
#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::{hint::black_box, marker::PhantomData};

mod error;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(not(target_os = "macos"))]
mod unsupported;
#[cfg(not(target_os = "macos"))]
use unsupported as backend;

pub use error::{Error, ErrorKind};

/// Counter values of one thread at one instant, or a difference of two.
///
/// Every count is of user space only: the kernel's work for the thread,
/// in system calls, interrupts and page faults, is left out.
///
/// On a processor with two kinds of cores, such as an M5 Pro, macOS
/// cannot bind a thread to one kind, and a reading counts the thread on
/// whichever kind it ran. The instructions and the branches do not
/// depend on the kind of core; the cycles and the branch misses do, so
/// there they may mix both kinds. [`cpu_number`] at both ends of a block
/// tells where it began and ended.
///
/// With the feature `serde`, a reading serializes as its four named
/// counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Reading {
    /// Retired instructions.
    pub instructions: u64,
    /// Clock cycles of the core while the thread ran in user space.
    pub cycles: u64,
    /// Retired branches that were mispredicted.
    pub branch_misses: u64,
    /// Retired branch instructions, calls and returns among them.
    pub branches: u64,
}

impl std::ops::Add for Reading {
    type Output = Reading;

    fn add(self, other: Reading) -> Reading {
        Reading {
            instructions: self.instructions + other.instructions,
            cycles: self.cycles + other.cycles,
            branch_misses: self.branch_misses + other.branch_misses,
            branches: self.branches + other.branches,
        }
    }
}

impl std::ops::AddAssign for Reading {
    fn add_assign(&mut self, other: Reading) {
        *self = *self + other;
    }
}

impl Reading {
    /// Counts accumulated since `earlier`, saturating at zero per field.
    pub fn since(self, earlier: Reading) -> Reading {
        Reading {
            instructions: self.instructions.saturating_sub(earlier.instructions),
            cycles: self.cycles.saturating_sub(earlier.cycles),
            branch_misses: self.branch_misses.saturating_sub(earlier.branch_misses),
            branches: self.branches.saturating_sub(earlier.branches),
        }
    }
}

/// The counters of the thread that opened them.
///
/// [`Counters::open`] sets the four counters up, and [`Counters::read`]
/// gives their counts since then. They count the thread that called
/// `open`, on whichever core it runs, and are read on that thread. So
/// the type is neither `Send` nor `Sync`, and the compiler refuses a
/// read on another thread:
///
/// ```compile_fail
/// fn send<T: Send>(_: T) {}
/// fn moves(counters: pmu::Counters) {
///     send(counters);
/// }
/// ```
///
/// ```compile_fail
/// fn share<T: Sync>(_: &T) {}
/// fn shares(counters: &pmu::Counters) {
///     share(counters);
/// }
/// ```
///
/// A program that measures several threads opens counters in each.
pub struct Counters {
    backend: backend::Counters,
    // Counting belongs to the thread that opened the counters.
    _thread: PhantomData<*const ()>,
}

impl Counters {
    /// Sets up and starts the counting of retired instructions, cycles,
    /// retired branches and mispredicted retired branches, in user space,
    /// for the calling thread.
    ///
    /// # Errors
    ///
    /// Fails when the counters cannot count here, with an [`Error`] whose
    /// [`kind`](Error::kind) names the cause:
    /// [`Unsupported`](ErrorKind::Unsupported) on a system without a
    /// backend or under Rosetta, [`NoCounters`](ErrorKind::NoCounters) on
    /// a machine without a PMU, [`Permission`](ErrorKind::Permission)
    /// without the privileges the system asks for,
    /// [`Busy`](ErrorKind::Busy) while another task holds the counters,
    /// [`MissingEvent`](ErrorKind::MissingEvent) when the processor lacks
    /// an event, and [`Other`](ErrorKind::Other) for any other failure of
    /// the system. [`ErrorKind`] gives the fix of each.
    pub fn open() -> Result<Self, Error> {
        Ok(Self {
            backend: backend::Counters::open()?,
            _thread: PhantomData,
        })
    }

    /// The counts of the calling thread since [`Counters::open`].
    ///
    /// A read makes one call into the kernel and allocates nothing, so
    /// its own cost inside a measured block is small and the same each
    /// time.
    ///
    /// # Errors
    ///
    /// Fails with [`ErrorKind::Lost`] when the counters stopped counting
    /// since they were opened: a reading is exact, or there is none.
    pub fn read(&self) -> Result<Reading, Error> {
        self.backend.read()
    }

    /// The counts of `iters` calls of `f`: a read, the loop, and a read.
    ///
    /// The counts include the loop and the [`black_box`] that keeps every
    /// result of `f` alive, and the small fixed cost of the reads.
    ///
    /// # Errors
    ///
    /// Fails as [`Counters::read`] does.
    pub fn measure<R>(&self, iters: u64, mut f: impl FnMut() -> R) -> Result<Reading, Error> {
        let start = self.read()?;
        for _ in 0..iters {
            black_box(f());
        }
        Ok(self.read()?.since(start))
    }
}

/// The number of the CPU the calling thread runs on now, or `None` where
/// the system does not tell.
///
/// It is not part of a measurement, but it explains the cycles of a
/// block: each core keeps its own state of the branch predictor, and a
/// processor may have two kinds of cores.
pub fn cpu_number() -> Option<usize> {
    backend::cpu_number()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_per_field_and_saturates() {
        let earlier = Reading {
            instructions: 10,
            cycles: 20,
            branch_misses: 3,
            branches: 4,
        };
        let later = Reading {
            instructions: 15,
            cycles: 25,
            branch_misses: 2,
            branches: 9,
        };
        assert_eq!(
            later.since(earlier),
            Reading {
                instructions: 5,
                cycles: 5,
                branch_misses: 0,
                branches: 5,
            }
        );
        assert_eq!(earlier.since(earlier), Reading::default());
        let mut total = earlier;
        total += later;
        assert_eq!(
            total,
            Reading {
                instructions: 25,
                cycles: 45,
                branch_misses: 5,
                branches: 13,
            }
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_reading_serializes_as_its_four_named_counts() {
        let reading = Reading {
            instructions: 1,
            cycles: 2,
            branch_misses: 3,
            branches: 4,
        };
        let json = serde_json::to_value(reading).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "instructions": 1,
                "cycles": 2,
                "branch_misses": 3,
                "branches": 4,
            })
        );
        assert_eq!(serde_json::from_value::<Reading>(json).unwrap(), reading);
    }
}
