//! Bounded storage and timed blocks for a mutating filter.
//!
//! Each invocation gets an independent copy of the original selection. At most
//! 4096 masks are live, regardless of calibration. Reset, view construction and
//! validation occur before the clock starts; no masks are restored while timed.

use super::{self as filtering, Input};
use anyhow::Result;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};
use tessera_core::{ColumnReader, RowMask};

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

    pub fn time_scalar<C: ColumnReader<Value = i32>>(
        &mut self,
        input: &Input<'_, C>,
        iterations: usize,
    ) -> f64 {
        self.time(iterations, |this, count| {
            let nrows = this.nrows;
            let mut views: Vec<_> = this
                .reset(count)
                .map(|words| RowMask::try_new(nrows, words).unwrap())
                .collect();
            let start = Instant::now();
            for rows in &mut views {
                black_box(filtering::scalar(black_box(input), black_box(rows))).unwrap();
            }
            start.elapsed()
        })
    }

    pub fn time_reference<C>(
        &mut self,
        input: &Input<'_, C>,
        run: impl Fn(&Input<'_, C>, &mut [u64]) -> Result<()>,
        iterations: usize,
    ) -> f64 {
        self.time(iterations, |this, count| {
            let mut views: Vec<_> = this.reset(count).collect();
            let start = Instant::now();
            for rows in &mut views {
                black_box(run(black_box(input), black_box(rows))).unwrap();
            }
            start.elapsed()
        })
    }

    fn time(
        &mut self,
        iterations: usize,
        mut run: impl FnMut(&mut Self, usize) -> Duration,
    ) -> f64 {
        assert!(iterations > 0);
        let mut elapsed = Duration::ZERO;
        let mut remaining = iterations;
        while remaining != 0 {
            let count = remaining.min(BLOCK_SIZE);
            elapsed += run(self, count);
            black_box(&self.storage);
            remaining -= count;
        }
        elapsed.as_secs_f64() * 1e9 / iterations as f64
    }
}
