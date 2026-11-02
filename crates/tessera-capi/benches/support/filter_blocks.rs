//! Bounded storage and counted blocks for a mutating filter.
//!
//! Each invocation gets an independent copy of the original selection of its
//! input; a ring of inputs is taken in turn. At most 4096 masks are live,
//! regardless of the call count. Reset, view construction and validation
//! occur before the counters are read; no masks are restored while counting.

use super::{self as filtering, Input};
use anyhow::Result;
use pmu::Reading;
use std::hint::black_box;
use tessera_core::{ColumnReader, RowMask};

pub const BLOCK_SIZE: usize = 4096;

pub struct Masks {
    nrows: usize,
    originals: Vec<Vec<u64>>,
    storage: Vec<u64>,
}

impl Masks {
    /// The masks of calls over the inputs of a ring, whose selections are
    /// `originals`, taken in turn.
    pub fn new(nrows: usize, originals: &[&[u64]]) -> Result<Self> {
        let originals = originals
            .iter()
            .map(|original| {
                let mut original = original.to_vec();
                RowMask::try_new(nrows, &mut original)?;
                Ok(original)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            nrows,
            storage: vec![0; originals[0].len().max(1) * BLOCK_SIZE],
            originals,
        })
    }

    /// `count` fresh masks: the first input's, the next one's, and so on
    /// in turn, as the calls take the inputs.
    pub fn reset(&mut self, count: usize) -> impl Iterator<Item = &mut [u64]> {
        assert!(count <= BLOCK_SIZE);
        let words = self.originals[0].len();
        let stride = words.max(1); // Give zero-row masks distinct empty subslices.
        for (chunk, original) in self.storage[..count * stride]
            .chunks_exact_mut(stride)
            .zip(self.originals.iter().cycle())
        {
            chunk[..words].copy_from_slice(original);
        }
        self.storage[..count * stride]
            .chunks_exact_mut(stride)
            .map(move |chunk| &mut chunk[..words])
    }

    /// Counters accumulated by `iterations` library filter calls on fresh masks,
    /// over `inputs` in turn.
    pub fn run_scalar<C: ColumnReader<Value = super::Value>>(
        &mut self,
        read: &mut dyn FnMut() -> Result<Reading, pmu::Error>,
        inputs: &[Input<'_, C>],
        iterations: u64,
    ) -> Result<Reading> {
        self.run(read, iterations, |this, read, count| {
            let nrows = this.nrows;
            let mut views: Vec<_> = this
                .reset(count)
                .map(|words| RowMask::try_new(nrows, words).unwrap())
                .collect();
            let start = read()?;
            for (rows, input) in views.iter_mut().zip(inputs.iter().cycle()) {
                black_box(filtering::scalar(black_box(input), black_box(rows))).unwrap();
            }
            Ok(read()?.since(start))
        })
    }

    /// Counters accumulated by `iterations` reference filter calls on fresh
    /// masks, over `inputs` in turn.
    pub fn run_reference<C>(
        &mut self,
        read: &mut dyn FnMut() -> Result<Reading, pmu::Error>,
        inputs: &[Input<'_, C>],
        run: impl Fn(&Input<'_, C>, &mut [u64]) -> Result<()>,
        iterations: u64,
    ) -> Result<Reading> {
        self.run(read, iterations, |this, read, count| {
            let mut views: Vec<_> = this.reset(count).collect();
            let start = read()?;
            for (rows, input) in views.iter_mut().zip(inputs.iter().cycle()) {
                black_box(run(black_box(input), black_box(rows))).unwrap();
            }
            Ok(read()?.since(start))
        })
    }

    fn run(
        &mut self,
        read: &mut dyn FnMut() -> Result<Reading, pmu::Error>,
        iterations: u64,
        mut run: impl FnMut(
            &mut Self,
            &mut dyn FnMut() -> Result<Reading, pmu::Error>,
            usize,
        ) -> Result<Reading>,
    ) -> Result<Reading> {
        let mut total = Reading::default();
        let mut remaining = iterations;
        while remaining != 0 {
            let count = remaining.min(BLOCK_SIZE as u64) as usize;
            total += run(self, read, count)?;
            black_box(&self.storage);
            remaining -= count as u64;
        }
        Ok(total)
    }
}
