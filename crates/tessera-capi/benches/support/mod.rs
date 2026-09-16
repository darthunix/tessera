//! Small building blocks shared by benchmarks and their deterministic tests.
//!
//! The runner measures PMU counters per operation and persists raw block
//! readings. Operation modules supply cases and measured functions; revision
//! comparison lives in tessera-bench.
#![allow(dead_code)] // Each target uses only its own operations.

pub mod fixture;
pub mod reference;
pub mod runner;
