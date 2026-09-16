//! Bounded storage and counted blocks for a mutating filter.
//!
//! Each invocation gets an independent copy of the original selection. At most
//! 4096 masks are live, regardless of the call count. Reset, view construction
//! and validation occur before the counters are read; no masks are restored
//! while counting.

use super::{self as filtering, Input};
use anyhow::Result;
use std::hint::black_box;
use tessera_core::{ColumnReader, RowMask};
use tessera_pmu::Reading;

pub const BLOCK_SIZE: usize = 4096;

pub struct Masks {
    nrows: usize,
    original: Vec<u64>,
    storage: Vec<u64>,
}

impl Masks {
    pub fn new(nrows: usize, original: &[u64]) -> Result<Self> {
        let mut original = original.to_vec();
        RowMask::try_new(nrows, &mut original)?;
        Ok(Self {
            nrows,
            storage: vec![0; original.len().max(1) * BLOCK_SIZE],
            original,
        })
    }

    pub fn reset(&mut self, count: usize) -> impl Iterator<Item = &mut [u64]> {
        assert!(count <= BLOCK_SIZE);
        let words = self.original.len();
        let stride = words.max(1); // Give zero-row masks distinct empty subslices.
        for chunk in self.storage[..count * stride].chunks_exact_mut(stride) {
            chunk[..words].copy_from_slice(&self.original);
        }
        self.storage[..count * stride]
            .chunks_exact_mut(stride)
            .map(move |chunk| &mut chunk[..words])
    }

    /// Counters accumulated by `iterations` library filter calls on fresh masks.
    pub fn run_scalar<C: ColumnReader<Value = i32>>(
        &mut self,
        read: &mut dyn FnMut() -> Reading,
        input: &Input<'_, C>,
        iterations: u64,
    ) -> Reading {
        self.run(read, iterations, |this, read, count| {
            let nrows = this.nrows;
            let mut views: Vec<_> = this
                .reset(count)
                .map(|words| RowMask::try_new(nrows, words).unwrap())
                .collect();
            let start = read();
            for rows in &mut views {
                black_box(filtering::scalar(black_box(input), black_box(rows))).unwrap();
            }
            read().since(start)
        })
    }

    /// Counters accumulated by `iterations` reference filter calls on fresh masks.
    pub fn run_reference<C>(
        &mut self,
        read: &mut dyn FnMut() -> Reading,
        input: &Input<'_, C>,
        run: impl Fn(&Input<'_, C>, &mut [u64]) -> Result<()>,
        iterations: u64,
    ) -> Reading {
        self.run(read, iterations, |this, read, count| {
            let mut views: Vec<_> = this.reset(count).collect();
            let start = read();
            for rows in &mut views {
                black_box(run(black_box(input), black_box(rows))).unwrap();
            }
            read().since(start)
        })
    }

    fn run(
        &mut self,
        read: &mut dyn FnMut() -> Reading,
        iterations: u64,
        mut run: impl FnMut(&mut Self, &mut dyn FnMut() -> Reading, usize) -> Reading,
    ) -> Reading {
        let mut total = Reading::default();
        let mut remaining = iterations;
        while remaining != 0 {
            let count = remaining.min(BLOCK_SIZE as u64) as usize;
            total += run(self, read, count);
            black_box(&self.storage);
            remaining -= count as u64;
        }
        total
    }
}
