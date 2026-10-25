// The crate's page is its README.
#![doc = include_str!("../README.md")]
// Unsafe code lives only in the backends, which call the system; every
// block there states its invariants.
#![deny(unsafe_code)]
#![deny(missing_docs)]

use anyhow::Result;
use std::hint::black_box;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;

/// Counter values of one thread at one instant, or a difference of two.
///
/// With the feature `serde`, a reading serializes as its four named
/// counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Reading {
    /// Retired instructions.
    pub instructions: u64,
    /// Core clock cycles while the thread ran in user space
    /// (`CORE_ACTIVE_CYCLE` on macOS).
    pub cycles: u64,
    /// Architecturally executed branches that were mispredicted.
    pub branch_misses: u64,
    /// Retired branch instructions, including calls and returns.
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

/// Configured counters of this process. Reads apply to the calling thread.
pub struct Counters {
    #[cfg(target_os = "macos")]
    inner: macos::Counters,
}

impl Counters {
    /// Configure and start per-thread counting of instructions, cycles,
    /// branch misses and branches for this process.
    ///
    /// Fails with the reason when counters are unavailable: no supported
    /// platform, missing framework or event, or insufficient privileges.
    pub fn open() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                inner: macos::Counters::open()?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            anyhow::bail!("PMU counters are implemented only for macOS (kperf)")
        }
    }

    /// Read the calling thread's counters.
    pub fn read(&self) -> Reading {
        #[cfg(target_os = "macos")]
        {
            self.inner.read()
        }
        #[cfg(not(target_os = "macos"))]
        {
            unreachable!("Counters cannot be opened on this platform")
        }
    }

    /// Counts accumulated by `iters` calls of `f`, including the loop and the
    /// `black_box` that keeps every result alive.
    pub fn measure<R>(&self, iters: u64, mut f: impl FnMut() -> R) -> Reading {
        let start = self.read();
        for _ in 0..iters {
            black_box(f());
        }
        self.read().since(start)
    }
}

/// Number of the CPU the calling thread runs on right now (diagnostics).
pub fn cpu_number() -> usize {
    #[cfg(target_os = "macos")]
    {
        macos::cpu_number()
    }
    #[cfg(not(target_os = "macos"))]
    {
        usize::MAX
    }
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
