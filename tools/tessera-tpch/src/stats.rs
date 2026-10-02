//! The statistics of a timing: medians and minima per mode, the ratio of
//! the medians with a confidence interval from a bootstrap over the pairs,
//! and the geometric mean of the ratios over queries.

use serde::{Deserialize, Serialize};

use crate::rundir::splitmix;

/// Resamples of the bootstrap.
const RESAMPLES: usize = 2000;

/// The seed of the bootstrap: a fixed one, so that a report printed again
/// from run.json shows the same interval.
const SEED: u64 = 0x7E55_E7A0_0000_0001;

/// A query's times in both modes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub query: u8,
    pub pairs: usize,
    pub off_median: f64,
    pub on_median: f64,
    pub off_min: f64,
    pub on_min: f64,
    /// on / off of the medians: below one Tessera is faster.
    pub ratio: f64,
    /// The 95 % interval of the ratio.
    pub low: f64,
    pub high: f64,
}

/// How a query came out against the threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Faster,
    Slower,
    Even,
}

impl Summary {
    /// Summarizes pairs of times `(on, off)` in milliseconds.
    pub fn new(query: u8, pairs: &[(f64, f64)]) -> Summary {
        let on: Vec<f64> = pairs.iter().map(|pair| pair.0).collect();
        let off: Vec<f64> = pairs.iter().map(|pair| pair.1).collect();
        let (on_median, off_median) = (median(&on), median(&off));
        let (low, high) = bootstrap(pairs);
        Summary {
            query,
            pairs: pairs.len(),
            off_median,
            on_median,
            off_min: off.iter().copied().fold(f64::INFINITY, f64::min),
            on_min: on.iter().copied().fold(f64::INFINITY, f64::min),
            ratio: on_median / off_median,
            low,
            high,
        }
    }

    /// Faster or slower by more than `threshold` percent, else even.
    pub fn outcome(&self, threshold: f64) -> Outcome {
        let threshold = threshold / 100.0;
        if self.ratio < 1.0 - threshold {
            Outcome::Faster
        } else if self.ratio > 1.0 + threshold {
            Outcome::Slower
        } else {
            Outcome::Even
        }
    }
}

pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// The 2.5 and 97.5 percentiles of the ratio of medians over resamples of
/// the pairs, which keep each pair's two times together: the drift of the
/// machine between pairs then stays out of the ratio.
fn bootstrap(pairs: &[(f64, f64)]) -> (f64, f64) {
    if pairs.len() < 2 {
        let ratio = pairs.first().map_or(f64::NAN, |pair| pair.0 / pair.1);
        return (ratio, ratio);
    }
    let mut state = SEED;
    let mut on = vec![0.0; pairs.len()];
    let mut off = vec![0.0; pairs.len()];
    let mut ratios: Vec<f64> = (0..RESAMPLES)
        .map(|_| {
            for slot in 0..pairs.len() {
                let pair = pairs[(splitmix(&mut state) % pairs.len() as u64) as usize];
                on[slot] = pair.0;
                off[slot] = pair.1;
            }
            median(&on) / median(&off)
        })
        .collect();
    ratios.sort_by(f64::total_cmp);
    let at = |quantile: f64| ratios[((RESAMPLES - 1) as f64 * quantile).round() as usize];
    (at(0.025), at(0.975))
}

/// The whole run: the geometric mean of the ratios over every query and
/// over those with Tessera nodes, the sums of the medians, and the count
/// of queries faster, slower and even.
#[derive(Debug, Clone, PartialEq)]
pub struct Totals {
    pub queries: usize,
    pub geomean: Option<f64>,
    pub tessera: usize,
    pub geomean_tessera: Option<f64>,
    pub off_ms: f64,
    pub on_ms: f64,
    pub faster: usize,
    pub slower: usize,
    pub even: usize,
}

impl Totals {
    pub fn new(summaries: &[Summary], with_tessera: impl Fn(u8) -> bool, threshold: f64) -> Totals {
        let count = |outcome| {
            summaries
                .iter()
                .filter(|summary| summary.outcome(threshold) == outcome)
                .count()
        };
        Totals {
            queries: summaries.len(),
            geomean: geometric_mean(summaries.iter().map(|summary| summary.ratio)),
            tessera: summaries
                .iter()
                .filter(|summary| with_tessera(summary.query))
                .count(),
            geomean_tessera: geometric_mean(
                summaries
                    .iter()
                    .filter(|summary| with_tessera(summary.query))
                    .map(|summary| summary.ratio),
            ),
            off_ms: summaries.iter().map(|summary| summary.off_median).sum(),
            on_ms: summaries.iter().map(|summary| summary.on_median).sum(),
            faster: count(Outcome::Faster),
            slower: count(Outcome::Slower),
            even: count(Outcome::Even),
        }
    }
}

pub fn geometric_mean(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values
        .into_iter()
        .fold((0.0, 0usize), |(sum, count), value| {
            (sum + value.ln(), count + 1)
        });
    (count > 0).then(|| (sum / count as f64).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert!(median(&[]).is_nan());
    }

    #[test]
    fn summaries_of_pairs() {
        let pairs = [
            (50.0, 100.0),
            (52.0, 101.0),
            (49.0, 99.0),
            (51.0, 100.0),
            (50.0, 102.0),
        ];
        let summary = Summary::new(6, &pairs);
        assert_eq!(summary.on_median, 50.0);
        assert_eq!(summary.off_median, 100.0);
        assert_eq!(summary.on_min, 49.0);
        assert_eq!(summary.off_min, 99.0);
        assert_eq!(summary.ratio, 0.5);
        assert!(summary.low <= 0.5 && 0.5 <= summary.high, "{summary:?}");
        assert!(summary.low > 0.48 && summary.high < 0.53, "{summary:?}");
        assert_eq!(summary.outcome(3.0), Outcome::Faster);
        // The same pairs give the same interval.
        assert_eq!(Summary::new(6, &pairs), summary);
    }

    #[test]
    fn outcomes_against_the_threshold() {
        let at = |ratio| Summary {
            ratio,
            ..Summary::new(1, &[(1.0, 1.0)])
        };
        assert_eq!(at(0.96).outcome(3.0), Outcome::Faster);
        assert_eq!(at(0.98).outcome(3.0), Outcome::Even);
        assert_eq!(at(1.02).outcome(3.0), Outcome::Even);
        assert_eq!(at(1.04).outcome(3.0), Outcome::Slower);
        assert_eq!(at(1.04).outcome(5.0), Outcome::Even);
    }

    #[test]
    fn a_noisy_ratio_has_a_wide_interval() {
        let pairs = [
            (50.0, 100.0),
            (150.0, 100.0),
            (60.0, 100.0),
            (140.0, 100.0),
            (100.0, 100.0),
        ];
        let summary = Summary::new(1, &pairs);
        assert!(summary.low < 0.7 && summary.high > 1.3, "{summary:?}");
    }

    #[test]
    fn totals_of_a_run() {
        let a = Summary::new(1, &[(50.0, 100.0)]);
        let b = Summary::new(6, &[(200.0, 100.0)]);
        let c = Summary::new(9, &[(101.0, 100.0)]);
        let totals = Totals::new(&[a, b, c], |query| query != 9, 3.0);
        assert_eq!((totals.queries, totals.tessera), (3, 2));
        assert!((totals.geomean_tessera.unwrap() - 1.0).abs() < 1e-12);
        assert_eq!((totals.off_ms, totals.on_ms), (300.0, 351.0));
        assert_eq!((totals.faster, totals.slower, totals.even), (1, 1, 1));
    }

    #[test]
    fn geometric_means() {
        assert!((geometric_mean([0.5, 2.0]).unwrap() - 1.0).abs() < 1e-12);
        assert!((geometric_mean([0.25, 1.0]).unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(geometric_mean([]), None);
    }
}
