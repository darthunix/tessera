//! The proofs by Kani of the arithmetic of a level's plan, for every
//! shape and level a host may give. A float of the bytes a level expects
//! weighs a count of partitions that is any power of two only at a cost
//! the proofs cannot pay, so the rule of doubling is proved whole for the
//! shapes that start from one partition, which try every count, and
//! without the expected bytes for the others.

use super::{LEAST_RECORDS, Level, Shape, below_limit, plan};
use crate::shared_spill::words_for;

/// Any shape.
fn any_shape() -> Shape {
    Shape {
        reserve: kani::any(),
        min_partitions: kani::any(),
        max_partitions: kani::any(),
        share: kani::any(),
        min_chunk: kani::any(),
        max_chunk: kani::any(),
        record: kani::any(),
    }
}

/// Any level, of the partitions given.
fn any_level(partitions: u32) -> Level {
    Level {
        expected: kani::any(),
        limit: kani::any(),
        shift: kani::any(),
        at_least: kani::any(),
        partitions,
    }
}

/// Whether a level may double `partitions` by every condition [`plan`]
/// documents but the bytes the level expects: they are fewer than the
/// most, each reserve of twice as many fits in half of the limit, and the
/// bits of twice as many, after `shift`, stay below 32.
fn may_double(shape: &Shape, level: &Level, partitions: u32) -> bool {
    partitions < shape.max_partitions
        && (partitions as usize)
            .checked_mul(2)
            .and_then(|count| count.checked_mul(shape.reserve))
            .is_some_and(|bytes| bytes <= level.limit / 2)
        && level.shift + (2 * u64::from(partitions)).trailing_zeros() < 32
}

/// Whether a level doubles `partitions`, as [`plan`] documents it: it may
/// double them, and they are fewer than `at_least` or, each holding half
/// of the limit, hold less than the level expects.
fn doubles(shape: &Shape, level: &Level, partitions: u32) -> bool {
    let half = level.limit / 2;
    may_double(shape, level, partitions)
        && (partitions < level.at_least || f64::from(partitions) * (half as f64) < level.expected)
}

/// The partitions a level plans, of any shape and level, are a power of
/// two within the shape's bounds, their bits after `shift` within the
/// hash's 32; and each count before them could double by every rule but
/// the expected bytes.
#[kani::proof]
#[kani::unwind(33)]
fn a_level_plans_partitions_of_its_shape() {
    let (shape, level) = (any_shape(), any_level(0));
    let Ok(got) = plan(&shape, &level) else {
        return;
    };
    let partitions = got.partitions;
    kani::cover!(
        partitions > shape.min_partitions,
        "a level doubles its partitions"
    );
    assert!(partitions.is_power_of_two());
    assert!(shape.min_partitions <= partitions && partitions <= shape.max_partitions);
    assert!(level.shift < 32 && level.shift + partitions.trailing_zeros() <= 32);
    for bit in 0..32 {
        let fewer = 1_u32 << bit;
        if shape.min_partitions <= fewer && fewer < partitions {
            assert!(may_double(&shape, &level, fewer));
        }
    }
}

/// A level of a shape that starts from `least` partitions, of any other
/// bounds and any level, plans the first count that does not double by
/// the rule [`plan`] documents, each count before it doubling.
fn plans_the_first_that_does_not_double(least: u32) {
    let mut shape = any_shape();
    shape.min_partitions = least;
    let level = any_level(0);
    let Ok(got) = plan(&shape, &level) else {
        return;
    };
    kani::cover!(
        got.partitions > 2 * least,
        "a level doubles its partitions twice"
    );
    for bit in 0..32 {
        let count = 1_u32 << bit;
        if least <= count && count < got.partitions {
            assert!(doubles(&shape, &level, count));
        } else if count == got.partitions {
            assert!(!doubles(&shape, &level, count));
        }
    }
}

/// The rule of doubling whole, from one partition, which tries every
/// count.
#[kani::proof]
#[kani::unwind(33)]
fn a_level_from_one_partition_plans_the_first_that_does_not_double() {
    plans_the_first_that_does_not_double(1);
}

/// The rule of doubling whole, from four partitions, as the nodes' joins
/// and groupings start.
#[kani::proof]
#[kani::unwind(33)]
fn a_level_from_four_partitions_plans_the_first_that_does_not_double() {
    plans_the_first_that_does_not_double(4);
}

/// A plan that passes, for any shape and any partitions given, takes them
/// as they are, with their bits after `shift` within the hash's 32; its
/// chunks are a multiple of 8, no shorter than the least of the shape, no
/// longer than the longest unless that is shorter than the least; its
/// words count the partitions; and a level below starts from the bit past
/// them, when the shape's least partitions, two or more, fit there.
/// Nothing overflows on the way.
#[kani::proof]
#[kani::unwind(33)]
fn a_plan_sizes_the_partitions_given() {
    let given: u32 = kani::any();
    kani::assume(given != 0);
    let (shape, level) = (any_shape(), any_level(given));
    let Ok(got) = plan(&shape, &level) else {
        return;
    };
    let partitions = got.partitions;
    kani::cover!(got.deeper, "a level fits below");
    assert!(partitions == given);
    assert!(level.shift < 32 && level.shift + partitions.trailing_zeros() <= 32);
    let least = shape.min_chunk.max(
        LEAST_RECORDS
            .saturating_mul(shape.record)
            .saturating_add(relhash::CHUNK_HEADER),
    );
    assert!(got.chunk_len.is_multiple_of(8) && got.chunk_len >= least);
    assert!(got.chunk_len <= shape.max_chunk || got.chunk_len == least);
    assert!(words_for(partitions as usize) == Ok(got.words));
    assert!(got.shift == level.shift && got.below == level.shift + partitions.trailing_zeros());
    let fits_below = got.below < 32 && got.below + shape.min_partitions.trailing_zeros() <= 32;
    assert!(got.deeper == (shape.min_partitions > 1 && fits_below));
}

/// A level below takes, of any limit, what the levels above leave of it
/// while they hold any bytes, while that is more than a quarter of the
/// limit; else a quarter of it.
#[kani::proof]
fn a_level_below_takes_what_is_left_a_quarter_at_least() {
    let (limit, held): (u64, u64) = (kani::any(), kani::any());
    let taken = below_limit(limit, held);
    if held < limit / 4 * 3 {
        assert!(taken == limit - held);
    } else {
        assert!(taken == limit / 4);
    }
}
