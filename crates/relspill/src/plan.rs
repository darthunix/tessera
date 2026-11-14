//! The plan of a level of a spill: how many partitions it splits into,
//! how long the chunks each keeps in memory are, how many spill words
//! count them, and which bits of the hash it takes. One rule plans the
//! levels of every node; what differs from node to node is the node's
//! [`Shape`], and what differs from level to level its [`Level`].
//!
//! A level takes the low bits of a row's hash: the first level from bit
//! 0, each level below from the bit past those of the level above it. A
//! row's partition is `(hash >> shift) & (partitions - 1)`. The buckets
//! of a `relhash` table take the high bits, so the rows of one partition
//! still spread over the buckets of the table that holds them. A host
//! that keeps rows in a table whose buckets take the low bits, such as a
//! dictionary of values, mixes the hashes it gives a level first.
//!
//! ```
//! use relspill::plan::{Level, Shape, expected, plan};
//!
//! // A node's spill: 4 to 1024 partitions, each keeping 48 KiB at least,
//! // and chunks of a sixteenth of the limit among them, from 8 KiB to
//! // 1 MiB, of four records of 40 bytes at least.
//! let shape = Shape {
//!     reserve: 48 << 10,
//!     min_partitions: 4,
//!     max_partitions: 1024,
//!     share: 16,
//!     min_chunk: 8 << 10,
//!     max_chunk: 1 << 20,
//!     record: 40,
//! };
//! // A table of 3 MiB in 30 000 rows of the 100 000 the planner expects.
//! let first = plan(&shape, &Level {
//!     expected: expected(f64::from(3 << 20), 30_000.0, 100_000.0, 1),
//!     limit: 4 << 20,
//!     shift: 0,
//!     at_least: 0,
//!     partitions: 0,
//! })?;
//! assert_eq!((first.partitions, first.chunk_len), (8, 32 << 10));
//! // A partition of 5 MiB read back splits by the next three bits.
//! let below = plan(&shape, &Level {
//!     expected: f64::from(5 << 20),
//!     shift: first.below,
//!     ..Level::first(4 << 20)
//! })?;
//! assert_eq!((below.shift, below.partitions), (3, 4));
//! # Ok::<(), relspill::Error>(())
//! ```

use crate::error::ensure;
use crate::shared_spill::words_for;
use crate::{Error, Result, Rule};

/// The least records a chunk of records holds, however long they are.
pub const LEAST_RECORDS: usize = 4;

/// What every level of one node's spill shares: the bounds of its
/// partitions and of its chunks. Its layout is fixed, as part of a
/// join's [`Params`](crate::join::Params).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// What each partition keeps in memory at the least, in bytes: a level
    /// doubles its partitions only while each reserve of twice as many
    /// fits in half of its limit.
    pub reserve: usize,
    /// The partitions to start from and the most, powers of two, the
    /// first no larger; the same for a node of a fixed count.
    pub min_partitions: u32,
    pub max_partitions: u32,
    /// The chunks: the limit shared among `share` times the partitions, no
    /// longer than `max_chunk`, and no shorter than `min_chunk` nor than a
    /// chunk's header, [`relhash::CHUNK_HEADER`], and [`LEAST_RECORDS`]
    /// records of `record` bytes, which win over `max_chunk`.
    pub share: usize,
    pub min_chunk: usize,
    pub max_chunk: usize,
    /// The bytes of a record of the level's chunks; 0 for chunks that hold
    /// no records.
    pub record: usize,
}

/// A level about to be made.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    /// The bytes the level is expected to hold: [`expected`] for the first
    /// level, the bytes of the partition it splits for a level below.
    pub expected: f64,
    /// The memory the level may take: `hash_mem`, or what the levels above
    /// leave of it.
    pub limit: usize,
    /// The hash bits the levels above took: 0 for the first level, the
    /// [`Plan::below`] of the level above for a level below it.
    pub shift: u32,
    /// Partitions at least, such as two per participant of a shared
    /// table; 0 for none.
    pub at_least: u32,
    /// The partitions another process planned for this level, which it
    /// takes as they are; 0 to plan them.
    pub partitions: u32,
}

impl Level {
    /// A first level of `limit` that expects nothing: the least
    /// partitions, or the only one of a shape of one.
    pub const fn first(limit: usize) -> Self {
        Self {
            expected: 0.0,
            limit,
            shift: 0,
            at_least: 0,
            partitions: 0,
        }
    }
}

/// The plan of a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Its partitions, a power of two within its shape's bounds.
    pub partitions: u32,
    /// The length of its chunks, a multiple of 8.
    pub chunk_len: usize,
    /// The spill words that count its partitions'
    /// [bytes](crate::shared_spill::Spill), [`words_for`] them.
    pub words: usize,
    /// The first bit of the hash it takes, its level's `shift`, below 32;
    /// it takes as many as its partitions need.
    pub shift: u32,
    /// The `shift` of a level below: the bit past its own.
    pub below: u32,
    /// Whether a level of its shape fits below: the shape's least
    /// partitions, two or more, in the bits from `below`. A shape of one
    /// partition takes no bits, so a level below it would split nothing:
    /// it never has one, and its plan only sizes its chunks.
    pub deeper: bool,
}

/// The plan of a level of a node's spill.
///
/// The partitions start from the shape's least and double while all of
/// these hold: they are fewer than its most; fewer than `at_least`, or,
/// each holding half of the limit, they hold less than the level expects;
/// each reserve of twice as many fits in half of the limit; and the bits
/// of twice as many, after `shift`, stay below 32. A level given its
/// partitions takes them as they are. The chunks then take the limit
/// shared among `share` times the partitions, within their bounds,
/// rounded down to a multiple of 8.
///
/// A wrong call: least and most partitions that are not powers of two in
/// order ([`Rule::PlanPartitions`]); expected bytes below zero or not a
/// number ([`Rule::Expected`]); a `shift` of 32 or more, or one after
/// which the least partitions do not fit in the hash ([`Rule::PlanBits`]);
/// given
/// partitions that are no power of two of the shape's
/// ([`Rule::GivenPartitions`]); a `share` of 0 ([`Rule::Share`]); a least
/// chunk, the longer of `min_chunk` and that of the records, that is not
/// a multiple of 8, so that the rounding would go below it
/// ([`Rule::LeastChunk`]); and more partitions than spill words count
/// ([`Rule::SpillPartitions`]).
pub fn plan(shape: &Shape, level: &Level) -> Result<Plan> {
    let partitions = partitions(shape, level)?;
    let below = level.shift + partitions.trailing_zeros();
    Ok(Plan {
        partitions,
        chunk_len: chunk_len(shape, level.limit, partitions)?,
        words: words_for(partitions as usize)?,
        shift: level.shift,
        below,
        deeper: shape.min_partitions > 1 && fits(shape.min_partitions, below),
    })
}

/// The bytes the first level of a spill expects: twice what the rows so
/// far take, `held` bytes in `rows` rows, or what `estimate` rows would
/// take at as many bytes a row, if more; for each of `participants` that
/// build one table, as each holds as much.
pub fn expected(held: f64, rows: f64, estimate: f64, participants: u32) -> f64 {
    let mut expected = held * 2.0;
    if rows > 0.0 {
        expected = expected.max(held / rows * estimate);
    }
    expected * f64::from(participants)
}

/// Whether `partitions`, a power of two, fit in the hash after `shift`
/// bits, the first of them a bit of the hash.
fn fits(partitions: u32, shift: u32) -> bool {
    shift < 32 && shift <= 32 - partitions.trailing_zeros()
}

fn partitions(shape: &Shape, level: &Level) -> Result<u32> {
    ensure!(
        shape.min_partitions.is_power_of_two()
            && shape.max_partitions.is_power_of_two()
            && shape.min_partitions <= shape.max_partitions,
        Error::wrong_call(Rule::PlanPartitions)
            .with(shape.min_partitions)
            .and(shape.max_partitions)
    );
    // Not below zero, and not a number refused too.
    let counted = level.expected >= 0.0;
    ensure!(counted, Error::wrong_call(Rule::Expected));
    ensure!(
        fits(shape.min_partitions, level.shift),
        Error::wrong_call(Rule::PlanBits)
            .with(shape.min_partitions)
            .and(level.shift)
    );
    if level.partitions != 0 {
        let given = level.partitions;
        ensure!(
            given.is_power_of_two()
                && (shape.min_partitions..=shape.max_partitions).contains(&given)
                && fits(given, level.shift),
            Error::wrong_call(Rule::GivenPartitions)
                .with(given)
                .and(level.shift)
        );
        return Ok(given);
    }
    let half = level.limit / 2;
    let mut partitions = shape.min_partitions;
    while partitions < shape.max_partitions
        && (partitions < level.at_least || f64::from(partitions) * (half as f64) < level.expected)
        && (partitions as usize)
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_mul(shape.reserve))
            .is_some_and(|bytes| bytes <= half)
        && level.shift + (31 - partitions.leading_zeros()) + 1 < 32
    {
        partitions *= 2;
    }
    Ok(partitions)
}

fn chunk_len(shape: &Shape, limit: usize, partitions: u32) -> Result<usize> {
    ensure!(
        shape.share > 0,
        Error::wrong_call(Rule::Share)
            .with(shape.share)
            .and(partitions)
    );
    let least = shape.min_chunk.max(
        LEAST_RECORDS
            .saturating_mul(shape.record)
            .saturating_add(relhash::CHUNK_HEADER),
    );
    ensure!(
        least.is_multiple_of(8),
        Error::wrong_call(Rule::LeastChunk).with(least)
    );
    let parts = shape.share.saturating_mul(partitions as usize);
    Ok((limit / parts).min(shape.max_chunk).max(least) & !7)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule of a wrong call, which every refusal here is.
    fn wrong(error: Error) -> Rule {
        assert_eq!(error.kind(), crate::Kind::WrongCall, "{error}");
        error.rule()
    }

    /// A level's partitions and chunk length, as the old rules gave them.
    #[derive(Debug, PartialEq, Eq)]
    struct Old {
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
    fn grouping(expected: f64, limit: usize, shift: u32, record: usize) -> Old {
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
        Old {
            partitions: n,
            chunk_len: chunk_len & !7,
        }
    }

    /// The join's rule as hashjoin_spill.c had it (join_spill_create),
    /// after the limit a level below gets.
    fn join(expected: f64, limit: usize, shift: u32, forced: u32, record: usize) -> Old {
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
        Old {
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

    /// A shape of 4 to 1024 partitions, chunks of 8 KiB to 1 MiB.
    fn shape(reserve: usize, share: usize, record: usize) -> Shape {
        Shape {
            reserve,
            min_partitions: 4,
            max_partitions: 1024,
            share,
            min_chunk: MIN_CHUNK,
            max_chunk: 1 << 20,
            record,
        }
    }

    fn level(expected: f64, limit: usize, shift: u32, at_least: u32) -> Level {
        Level {
            expected,
            limit,
            shift,
            at_least,
            partitions: 0,
        }
    }

    /// The rest of a plan as the nodes had it: `table_spill_words` of the
    /// partitions, and a level below after the shift and their bits.
    fn same_rest(plan: Plan, shift: u32) -> Result<()> {
        assert_eq!(plan.words, words_for(plan.partitions as usize)?);
        assert_eq!(plan.shift, shift);
        assert_eq!(plan.below, shift + bit(plan.partitions));
        Ok(())
    }

    fn old(plan: Plan) -> Old {
        Old {
            partitions: plan.partitions,
            chunk_len: plan.chunk_len,
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
                        let level = level(expected, limit, shift, 0);
                        let got = plan(&shape(MIN_CHUNK + PAGE, 8, record), &level)?;
                        assert_eq!(old(got), grouping(expected, limit, shift, record));
                        same_rest(got, shift)?;
                        let join_shape = shape(join_reserve, 16, record);
                        let got = plan(&join_shape, &level)?;
                        assert_eq!(old(got), join(expected, limit, shift, 0, record));
                        same_rest(got, shift)?;
                        if shift == 0 {
                            for forced in [4, 64] {
                                let given = Level {
                                    partitions: forced,
                                    ..level
                                };
                                let got = plan(&join_shape, &given)?;
                                assert_eq!(old(got), join(expected, limit, 0, forced, record));
                                same_rest(got, 0)?;
                            }
                        }
                    }
                }
                for participants in 1..=9 {
                    let level = level(expected, limit, 0, 2 * participants);
                    let got = plan(&shape(join_reserve, 16, 0), &level)?.partitions;
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
        let shape = shape(48 << 10, 16, 0);
        let level = level(f64::from(10 << 20), 4 << 20, 0, 0);
        let partitions = |level| plan(&shape, &level).map(|plan| plan.partitions);
        assert_eq!(partitions(level)?, 8);
        // Five words of the head and five for each partition and the
        // partitions kept in memory.
        assert_eq!(plan(&shape, &level)?.words, 50);
        assert_eq!(
            partitions(Level {
                at_least: 16,
                ..level
            })?,
            16
        );
        assert_eq!(
            partitions(Level {
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
        let after = |shift| plan(&shape(0, 16, 0), &level(1e15, 4 << 20, shift, 0));
        assert_eq!(after(28)?.partitions, 8);
        assert_eq!(after(29)?.partitions, 4);
        assert_eq!(after(30)?.partitions, 4);
        assert_eq!(after(31).map_err(wrong), Err(Rule::PlanBits));
        Ok(())
    }

    /// Each level takes the bits past those of the level above, and says
    /// whether a level fits below it: a shape of 4 partitions or more does
    /// below 30 bits, not below 31; one of 32 fixed partitions, as the
    /// grouping's rows, makes six levels, five bits each from bit 0; one
    /// of one partition has none below it.
    #[test]
    fn each_level_takes_the_bits_past_those_above() -> Result<()> {
        let shape = shape(0, 16, 0);
        let first = plan(&shape, &level(f64::from(10 << 20), 4 << 20, 0, 0))?;
        assert_eq!((first.shift, first.partitions, first.below), (0, 8, 3));
        let below = plan(&shape, &level(1e15, 4 << 20, first.below, 0))?;
        assert_eq!((below.shift, below.below), (3, 3 + 10));
        let deep = plan(&shape, &level(1e15, 4 << 20, 20, 0))?;
        assert_eq!((deep.partitions, deep.below, deep.deeper), (1024, 30, true));
        let last = plan(&shape, &level(1e15, 4 << 20, 28, 0))?;
        assert_eq!((last.partitions, last.below, last.deeper), (8, 31, false));

        let rows = Shape {
            min_partitions: 32,
            max_partitions: 32,
            min_chunk: 4112,
            max_chunk: 4112,
            ..shape
        };
        let mut shift = 0;
        for at in 0..6 {
            let level = plan(&rows, &level(1e15, 4 << 20, shift, 0))?;
            assert_eq!((level.partitions, level.shift), (32, 5 * at));
            assert_eq!(level.chunk_len, 4112);
            assert_eq!(level.deeper, at < 5, "level {at}");
            shift = level.below;
        }
        assert_eq!(
            plan(&rows, &level(0.0, 4 << 20, shift, 0)).map_err(wrong),
            Err(Rule::PlanBits)
        );

        // A shape of one partition takes no bits: no level below it, and
        // its first bit is a bit of the hash.
        let one = Shape {
            min_partitions: 1,
            max_partitions: 1,
            ..shape
        };
        let last = plan(&one, &level(0.0, 4 << 20, 31, 0))?;
        assert_eq!((last.partitions, last.below, last.deeper), (1, 31, false));
        assert!(!plan(&one, &Level::first(4 << 20))?.deeper);
        assert_eq!(
            plan(&one, &level(0.0, 4 << 20, 32, 0)).map_err(wrong),
            Err(Rule::PlanBits)
        );
        Ok(())
    }

    /// A level given its partitions takes them, and its chunks share the
    /// limit among them; partitions that are no power of two of the shape,
    /// or that pass the hash, are refused.
    #[test]
    fn a_level_takes_the_partitions_it_is_given() -> Result<()> {
        let shape = shape(0, 16, 0);
        let given = |partitions, shift| Level {
            partitions,
            ..level(0.0, 4 << 20, shift, 0)
        };
        // A sixteenth of 4 MiB among 64 is 4 KiB, below the least chunk.
        let got = plan(&shape, &given(64, 0))?;
        assert_eq!(
            (got.partitions, got.chunk_len, got.below),
            (64, MIN_CHUNK, 6)
        );
        assert_eq!(plan(&shape, &given(1024, 22))?.below, 32);
        for (partitions, shift) in [(2, 0), (48, 0), (2048, 0), (1024, 23)] {
            assert_eq!(
                plan(&shape, &given(partitions, shift)).map_err(wrong),
                Err(Rule::GivenPartitions),
                "{partitions} after {shift}"
            );
        }
        Ok(())
    }

    /// A chunk is a share of the limit among the partitions, within its
    /// bounds, rounded down to a multiple of 8.
    #[test]
    fn a_chunk_is_a_share_of_the_limit_within_its_bounds() -> Result<()> {
        let chunk = |limit, partitions, min_chunk| {
            let shape = Shape {
                min_chunk,
                max_partitions: 1024,
                ..shape(0, 16, 0)
            };
            let level = Level {
                partitions,
                ..level(0.0, limit, 0, 0)
            };
            plan(&shape, &level).map(|plan| plan.chunk_len)
        };
        assert_eq!(chunk(4 << 20, 8, 8 << 10)?, 32 << 10);
        assert_eq!(chunk(4 << 20, 1024, 8 << 10)?, 8 << 10, "the least");
        assert_eq!(chunk(1 << 30, 4, 8 << 10)?, 1 << 20, "the most");
        assert_eq!(chunk(1_000_000, 8, 8)?, 7808, "rounded");
        // Four records of a kilobyte win over the least chunk and the most.
        let wide = Shape {
            min_chunk: 8,
            max_chunk: 1024,
            ..shape(0, 16, 1024)
        };
        assert_eq!(plan(&wide, &level(0.0, 1 << 30, 0, 0))?.chunk_len, 4104);
        Ok(())
    }

    /// A node's table in memory takes one partition at a share of 8,
    /// between its first chunk, 64 kB, and the most, 1 MiB.
    #[test]
    fn a_table_in_memory_takes_an_eighth_of_the_limit() -> Result<()> {
        let table = Shape {
            reserve: 0,
            min_partitions: 1,
            max_partitions: 1,
            share: 8,
            min_chunk: 64 << 10,
            max_chunk: 1 << 20,
            record: 0,
        };
        for (limit, len) in [
            (256 << 10, 64 << 10),
            (512 << 10, 64 << 10),
            (524_352, 65_544),
            (3 << 20, 393_216),
            ((8 << 20) - 1, 1_048_568),
            (8 << 20, 1 << 20),
        ] {
            let got = plan(&table, &Level::first(limit))?;
            assert_eq!((got.partitions, got.chunk_len), (1, len), "{limit}");
        }
        Ok(())
    }

    /// The first level expects twice the bytes held, or the bytes the
    /// rows expected would take at the bytes a row so far, if more; for
    /// every participant.
    #[test]
    fn the_first_level_expects_twice_its_bytes_or_the_estimate() {
        assert_eq!(expected(1000.0, 10.0, 15.0, 1), 2000.0);
        assert_eq!(expected(1000.0, 10.0, 30.0, 1), 3000.0);
        assert_eq!(expected(1000.0, 0.0, 30.0, 1), 2000.0, "no rows so far");
        assert_eq!(expected(1000.0, 10.0, 30.0, 4), 12_000.0);
        assert_eq!(expected(0.0, 0.0, 1e9, 1), 0.0);
    }

    #[test]
    fn a_plan_refuses_bounds_out_of_order() -> Result<()> {
        let base = shape(0, 16, 0);
        let first = level(1e9, 1 << 30, 0, 0);
        let refused = |shape: Shape, level: Level| plan(&shape, &level).map_err(wrong);
        for (min_partitions, max_partitions) in [(3, 1024), (8, 4), (4, 1000), (0, 4)] {
            let shape = Shape {
                min_partitions,
                max_partitions,
                ..base
            };
            assert_eq!(refused(shape, first), Err(Rule::PlanPartitions));
        }
        assert_eq!(refused(Shape { share: 0, ..base }, first), Err(Rule::Share));
        // A least chunk not a multiple of 8, of its own or by its records.
        assert_eq!(
            refused(
                Shape {
                    min_chunk: 4100,
                    ..base
                },
                first
            ),
            Err(Rule::LeastChunk)
        );
        let odd = Shape {
            record: 4099,
            ..base
        };
        assert_eq!(refused(odd, first), Err(Rule::LeastChunk));
        // Partitions past those spill words count.
        let many = Shape {
            min_partitions: 1 << 17,
            max_partitions: 1 << 17,
            ..base
        };
        assert_eq!(refused(many, first), Err(Rule::SpillPartitions));
        // Expected bytes below zero or not a number.
        for expected in [-1.0, f64::NAN] {
            assert_eq!(
                refused(base, Level { expected, ..first }),
                Err(Rule::Expected)
            );
        }
        // Four partitions after 30 bits fit; after 31 they do not.
        assert_eq!(plan(&base, &Level { shift: 30, ..first })?.partitions, 4);
        for shift in [31, u32::MAX] {
            assert_eq!(refused(base, Level { shift, ..first }), Err(Rule::PlanBits));
        }
        // A reserve too large for any partition keeps the first count.
        let reserved = Shape {
            reserve: usize::MAX,
            ..base
        };
        assert_eq!(plan(&reserved, &first)?.partitions, 4);
        Ok(())
    }
}
