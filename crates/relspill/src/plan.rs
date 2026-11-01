//! How a level of a spill splits: its partitions and the length of the
//! chunks each keeps in memory. A grouping's spill, a join's and a shared
//! join table's split by these rules with their own parameters.

use anyhow::{Result, ensure};

/// What a level of a spill holds and has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// The bytes the level is expected to hold.
    pub expected: f64,
    /// The memory the level may take, hash_mem or what the levels above
    /// leave of it.
    pub limit: usize,
    /// What each partition keeps in memory at the least: its tails of
    /// chunks and its files' buffers.
    pub reserve: usize,
    /// The hash bits the levels above took.
    pub shift: u32,
    /// The partitions to start from and the most, powers of two.
    pub min_partitions: u32,
    pub max_partitions: u32,
    /// Partitions at least, such as two per participant of a shared
    /// table; 0 for none.
    pub at_least: u32,
}

/// The partitions of a level: the power of two that makes each hold
/// about half of `limit` of what the level expects, or `at_least` of them,
/// as long as each partition's reserve fits in half of `limit` and the
/// hash bits last. Expected bytes below zero or not a number, and a
/// `shift` past which the least partitions do not fit in the hash, are
/// refused.
pub fn partitions(level: &Level) -> Result<u32> {
    ensure!(
        level.min_partitions.is_power_of_two()
            && level.max_partitions.is_power_of_two()
            && level.min_partitions <= level.max_partitions,
        "{} to {} partitions are not powers of two in order",
        level.min_partitions,
        level.max_partitions
    );
    ensure!(
        level.expected >= 0.0,
        "a level that expects {} bytes",
        level.expected
    );
    ensure!(
        level.shift <= 32 - level.min_partitions.trailing_zeros(),
        "{} partitions do not fit in the hash after {} bits",
        level.min_partitions,
        level.shift
    );
    let half = level.limit / 2;
    let mut partitions = level.min_partitions;
    while partitions < level.max_partitions
        && (partitions < level.at_least || f64::from(partitions) * (half as f64) < level.expected)
        && (partitions as usize)
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_mul(level.reserve))
            .is_some_and(|bytes| bytes <= half)
        && level.shift + (31 - partitions.leading_zeros()) + 1 < 32
    {
        partitions *= 2;
    }
    Ok(partitions)
}

/// The length of a level's chunks: `limit / (share * partitions)` bytes,
/// no more than `max_chunk`, no less than `min_chunk` (which wins over
/// `max_chunk`: a chunk holds a few records however large), rounded down
/// to a multiple of 8. A `min_chunk` below 8, a chunk's header, or not a
/// multiple of 8 is refused, so the rounding never goes below it.
pub fn chunk_len(
    limit: usize,
    partitions: u32,
    share: usize,
    min_chunk: usize,
    max_chunk: usize,
) -> Result<usize> {
    ensure!(
        share > 0 && partitions > 0,
        "chunks of a {share} share of {partitions} partitions"
    );
    ensure!(
        min_chunk >= 8 && min_chunk.is_multiple_of(8),
        "a least chunk of {min_chunk} bytes"
    );
    let parts = share.saturating_mul(partitions as usize);
    Ok((limit / parts).min(max_chunk).max(min_chunk) & !7)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A level's partitions and chunk length, as the old rules gave them.
    #[derive(Debug, PartialEq, Eq)]
    struct Plan {
        partitions: u32,
        chunk_len: usize,
    }

    const PAGE: usize = 8192;
    const MIN_CHUNK: usize = 8 * 1024;
    const HEADER: usize = 8;

    fn bit(partitions: u32) -> u32 {
        31 - partitions.leading_zeros()
    }

    /// The grouping's rule as agg_spill.c had it (agg_spill_create).
    fn grouping(expected: f64, limit: usize, shift: u32, record: usize) -> Plan {
        let mut n: u32 = 4;
        while n < 1024
            && f64::from(n) * ((limit / 2) as f64) < expected
            && n as usize * 2 * (MIN_CHUNK + PAGE) <= limit / 2
            && shift + bit(n) + 1 < 32
        {
            n *= 2;
        }
        let mut chunk_len = limit / (8 * n as usize);
        chunk_len = chunk_len.min(1 << 20);
        chunk_len = chunk_len.max(MIN_CHUNK);
        chunk_len = chunk_len.max(HEADER + 4 * record);
        Plan {
            partitions: n,
            chunk_len: chunk_len & !7,
        }
    }

    /// The join's rule as hashjoin_spill.c had it (join_spill_create),
    /// after the limit a level below gets.
    fn join(expected: f64, limit: usize, shift: u32, forced: u32, record: usize) -> Plan {
        let mut n: u32 = 4;
        while n < 1024
            && f64::from(n) * ((limit / 2) as f64) < expected
            && n as usize * 2 * (4 * MIN_CHUNK + 2 * PAGE) <= limit / 2
            && shift + bit(n) + 1 < 32
        {
            n *= 2;
        }
        if forced > 0 {
            n = forced;
        }
        let mut chunk_len = limit / (16 * n as usize);
        chunk_len = chunk_len.min(1 << 20);
        chunk_len = chunk_len.max(MIN_CHUNK);
        chunk_len = chunk_len.max(HEADER + 4 * record);
        Plan {
            partitions: n,
            chunk_len: chunk_len & !7,
        }
    }

    /// The shared table's partitions as hashjoin_shared.c had them
    /// (shared_split): two per participant at least, no limit of bits.
    fn shared(expected: f64, limit: usize, participants: u32) -> u32 {
        let mut n: u32 = 4;
        while n < 1024
            && (n < 2 * participants || f64::from(n) * ((limit / 2) as f64) < expected)
            && n as usize * 2 * (4 * MIN_CHUNK + 2 * PAGE) <= limit / 2
        {
            n *= 2;
        }
        n
    }

    fn level(expected: f64, limit: usize, shift: u32, reserve: usize, at_least: u32) -> Level {
        Level {
            expected,
            limit,
            reserve,
            shift,
            min_partitions: 4,
            max_partitions: 1024,
            at_least,
        }
    }

    fn expectations() -> Vec<f64> {
        let mut out = vec![0., 1.];
        let mut value = 1000.;
        while value < 1e14 {
            out.extend([value, value * 1.5, value * 3.7]);
            value *= 10.;
        }
        out
    }

    const LIMITS: [usize; 9] = [
        64 * 1024,
        256 * 1024,
        1 << 20,
        4 << 20,
        64 << 20,
        256 << 20,
        1 << 30,
        8 << 30,
        (1 << 30) + 12345,
    ];

    #[test]
    fn the_three_rules_are_one_with_their_parameters() -> Result<()> {
        let join_reserve = 4 * MIN_CHUNK + 2 * PAGE;
        for expected in expectations() {
            for limit in LIMITS {
                for shift in [0, 5, 10, 20, 25, 27, 28, 29, 30] {
                    for record in [24, 40, 4096, 70_000] {
                        let min_chunk = MIN_CHUNK.max(HEADER + 4 * record);
                        let n = partitions(&level(expected, limit, shift, MIN_CHUNK + PAGE, 0))?;
                        let got = Plan {
                            partitions: n,
                            chunk_len: chunk_len(limit, n, 8, min_chunk, 1 << 20)?,
                        };
                        assert_eq!(got, grouping(expected, limit, shift, record));
                        for forced in [0, 4, 64] {
                            let mut n =
                                partitions(&level(expected, limit, shift, join_reserve, 0))?;
                            if forced > 0 {
                                n = forced;
                            }
                            let got = Plan {
                                partitions: n,
                                chunk_len: chunk_len(limit, n, 16, min_chunk, 1 << 20)?,
                            };
                            assert_eq!(got, join(expected, limit, shift, forced, record));
                        }
                    }
                }
                for participants in 1..=9 {
                    let got =
                        partitions(&level(expected, limit, 0, join_reserve, 2 * participants))?;
                    assert_eq!(got, shared(expected, limit, participants));
                }
            }
        }
        Ok(())
    }

    /// A level of 4 to 1024 partitions with a reserve of 48 KiB that
    /// expects 10 MiB: 8 partitions in a limit of 4 MiB, 16 when it wants
    /// 16 at the least, 4 in a limit of 256 KiB, where the reserves of 8
    /// pass half of it.
    #[test]
    fn a_level_doubles_by_its_bytes_participants_and_reserve() -> Result<()> {
        let level = Level {
            expected: f64::from(10 << 20),
            ..level(0.0, 4 << 20, 0, 48 << 10, 0)
        };
        assert_eq!(partitions(&level)?, 8);
        assert_eq!(
            partitions(&Level {
                at_least: 16,
                ..level
            })?,
            16
        );
        assert_eq!(
            partitions(&Level {
                limit: 256 << 10,
                ..level
            })?,
            4
        );
        Ok(())
    }

    /// A level that expects far more than its limit after 28, 29 and 30
    /// bits gets 8, 4 and 4 partitions: the bits of twice as many stay
    /// below 32.
    #[test]
    fn a_level_keeps_to_the_bits_of_the_hash() -> Result<()> {
        let after = |shift| level(1e15, 4 << 20, shift, 0, 0);
        assert_eq!(partitions(&after(28))?, 8);
        assert_eq!(partitions(&after(29))?, 4);
        assert_eq!(partitions(&after(30))?, 4);
        assert!(partitions(&after(31)).is_err());
        Ok(())
    }

    /// A chunk is a share of the limit among the partitions, within its
    /// bounds, rounded down to a multiple of 8.
    #[test]
    fn a_chunk_is_a_share_of_the_limit_within_its_bounds() -> Result<()> {
        assert_eq!(chunk_len(4 << 20, 8, 16, 8 << 10, 1 << 20)?, 32 << 10);
        assert_eq!(
            chunk_len(4 << 20, 1024, 16, 8 << 10, 1 << 20)?,
            8 << 10,
            "the least"
        );
        assert_eq!(
            chunk_len(1 << 30, 4, 16, 8 << 10, 1 << 20)?,
            1 << 20,
            "the most"
        );
        assert_eq!(chunk_len(1_000_000, 8, 16, 8, 1 << 20)?, 7808, "rounded");
        Ok(())
    }

    #[test]
    fn a_plan_refuses_bounds_out_of_order() -> Result<()> {
        let base = level(1e9, 1 << 30, 0, 0, 0);
        for (min_partitions, max_partitions) in [(3, 1024), (8, 4), (4, 1000)] {
            assert!(
                partitions(&Level {
                    min_partitions,
                    max_partitions,
                    ..base
                })
                .is_err()
            );
        }
        assert!(chunk_len(1 << 30, 4, 0, 8, 1 << 20).is_err());
        assert!(chunk_len(1 << 30, 0, 8, 8, 1 << 20).is_err());
        // A least chunk below a header, or not a multiple of 8.
        for min_chunk in [0, 4, 4100] {
            assert!(chunk_len(1 << 30, 4, 8, min_chunk, 1 << 20).is_err());
        }
        // Expected bytes below zero or not a number.
        for expected in [-1.0, f64::NAN] {
            assert!(partitions(&Level { expected, ..base }).is_err());
        }
        // Four partitions after 30 bits fit; after 31 they do not.
        assert_eq!(partitions(&Level { shift: 30, ..base })?, 4);
        assert!(partitions(&Level { shift: 31, ..base }).is_err());
        assert!(
            partitions(&Level {
                shift: u32::MAX,
                ..base
            })
            .is_err()
        );
        // The least chunk wins over the most.
        assert_eq!(chunk_len(1 << 30, 4, 8, 4096, 1024)?, 4096);
        // A reserve too large for any partition keeps the first count.
        assert_eq!(
            partitions(&Level {
                reserve: usize::MAX,
                ..base
            })?,
            4
        );
        Ok(())
    }
}
