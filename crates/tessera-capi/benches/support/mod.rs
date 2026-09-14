//! Small building blocks shared by benchmarks and their deterministic tests.
//!
//! The runner owns case traversal, sampling owns measurement order, and report
//! owns presentation. Each benchmark supplies its cases and timed operations.
#![allow(dead_code)] // Each target uses only its own operations and policies.

pub mod baseline;
pub mod fixture;
pub mod measurement;
pub mod options;
pub mod reference;
pub mod report;
pub mod runner;
pub mod sampling;
