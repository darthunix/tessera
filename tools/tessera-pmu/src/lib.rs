//! Per-thread PMU counters for benchmarking arbitrary code.
//!
//! Instructions retired are deterministic for a fixed input: they depend
//! neither on the core a thread happens to run on, nor on its frequency, nor
//! on predictor state. That gives nanosecond-scale operations a metric with
//! real resolution, which wall time lacks. Cycles are wall time without
//! frequency scaling; branch misses explain most cycle-only effects. Counting
//! is per thread and survives migration between cores.
//!
//! Only macOS through the private kperf frameworks is implemented, and it
//! needs root: run `sudo -v`, then start the measuring process with `sudo -n`.
//! [`Counters::open`] reports why counters are unavailable. There is no
//! fallback to timing.
//!
//! This crate is tooling: it is never linked into the library. It may use
//! `unsafe` for the kperf ABI; every block states its invariants.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::hint::black_box;

#[cfg(target_os = "macos")]
mod macos;

/// Counter values of one thread at one instant, or a difference of two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    /// Retired instructions.
    pub instructions: u64,
    /// Core clock cycles.
    pub cycles: u64,
    /// Architecturally executed branches that were mispredicted.
    pub branch_misses: u64,
    /// Retired branch instructions, including calls and returns.
    pub branches: u64,
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
    }
}
