//! Warmup, one-time calibration and paired sampling; never the timed operation.
//!
//! `time` selects and measures a whole segment. Dispatch happens outside that
//! segment's clock, so sharing this schedule adds no per-invocation indirection.

use super::measurement::{Measurement, SAMPLES, Sample};
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

/// One pass borrows the case's calibration and samples, retained across passes.
pub struct Series<'a> {
    pub index: usize,
    pub calibration: &'a mut Option<usize>,
    pub measurement: &'a mut Measurement,
}

impl Series<'_> {
    pub fn collect(
        self,
        paths: &[&'static str],
        mut time: impl FnMut(&'static str, usize) -> f64,
    ) -> Result<()> {
        let warmup = Instant::now();
        let estimate = loop {
            let estimate = time("control", 10_000);
            for &path in paths.iter().filter(|&&path| path != "control") {
                time(path, 10_000);
            }
            if warmup.elapsed() >= Duration::from_millis(30) {
                break estimate;
            }
        };
        ensure!(
            estimate.is_finite() && estimate > 0.,
            "invalid calibration duration"
        );
        let iterations = *self
            .calibration
            .get_or_insert_with(|| (10_000_000. / estimate).ceil().clamp(1., 20_000_000.) as usize);
        for index in 0..SAMPLES {
            for step in 0..paths.len() {
                let path = paths[(self.index + index + step) % paths.len()];
                let before_ns = time("control", iterations);
                let measured_ns = time(path, iterations);
                let after_ns = time("control", iterations);
                self.measurement.insert(
                    path,
                    self.index,
                    index,
                    Sample {
                        before_ns,
                        measured_ns,
                        after_ns,
                    },
                )?;
            }
        }
        Ok(())
    }
}
