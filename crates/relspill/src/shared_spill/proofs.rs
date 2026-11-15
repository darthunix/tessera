//! The proofs by Kani of the rule that sends partitions to disk, over
//! words that hold any values and any memory, with the weights the
//! nodes' settings give by default.

use core::cell::Cell;

use super::{Field, LocalSpill, Memory, ON_DISK, PARTITIONS, Weights, words_for};

/// The partitions in force of the proofs' words: four, so that the lowest
/// of several in memory is a choice.
const PARTS: u32 = 4;

/// The words of the proofs' spill.
const WORDS: usize = 5 + 5 * (PARTS as usize + 1);

/// The partition the rule sends, over the words of four partitions in
/// force that hold any bytes, records and flags, the given weights and
/// any memory, and whether the memory pressed; the rule sends
/// what [`Spill::evict`](super::Spill::evict) documents: none past a
/// check's limit; while the memory with the reserve of the partitions on
/// disk passes the share of the limit, the first partition of the most
/// bytes, as weighed; else, with a partition on disk and those in memory
/// holding fewer than the share of the records, the lowest in memory;
/// else none. The partition sent is marked on disk, and nothing panics
/// or overflows.
fn the_rule_sends_what_its_documentation_says(weights: Weights) -> (Option<u32>, bool) {
    assert!(words_for(PARTS as usize) == Ok(WORDS));
    let values: [u64; WORDS] = kani::any();
    let cells = values.map(Cell::new);
    cells[PARTITIONS].set(u64::from(PARTS));
    let Ok(spill) = LocalSpill::new(&cells) else {
        return (None, false);
    };
    let memory = if kani::any() {
        Memory::Counted
    } else {
        Memory::Measured {
            bytes: kani::any(),
            limit: kani::any(),
        }
    };
    let evicted: u32 = kani::any();
    let word = |partition: u32, field: Field| cells[spill.part(partition, field)].get();
    // The rule, as its documentation gives it, over the words before.
    let spilled: [bool; 4] = core::array::from_fn(|p| word(p as u32, Field::Flags) & ON_DISK != 0);
    let mut largest = None;
    let mut most = 0.0;
    let (mut on_disk, mut resident, mut total) = (0_u64, 0_u64, 0_u64);
    for p in 0..PARTS as usize {
        let weight = if spilled[p] { weights.spilled } else { 1.0 };
        let held = word(p as u32, Field::Bytes) as f64 * weight;
        if held > most {
            (largest, most) = (Some(p as u32), held);
        }
        let records = word(p as u32, Field::Records);
        total = total.saturating_add(records);
        if spilled[p] {
            on_disk += 1;
        } else {
            resident = resident.saturating_add(records);
        }
    }
    let lowest_in_memory = spilled.iter().position(|&on| !on).map(|p| p as u32);
    let (bytes, limit) = match memory {
        Memory::Counted => (spill.bytes(), cells[super::BUDGET].get()),
        Memory::Measured { bytes, limit } => (bytes, limit),
    };
    let share = if evicted == 0 {
        weights.start
    } else {
        weights.target
    };
    let pressed = bytes as f64 + weights.reserve as f64 * on_disk as f64 > share * limit as f64;
    let expected = if weights.per_check > 0 && evicted >= weights.per_check {
        None
    } else if pressed && largest.is_some() {
        largest
    } else if on_disk > 0 && resident > 0 && (resident as f64) < weights.resident * total as f64 {
        lowest_in_memory
    } else {
        None
    };
    let Ok(sent) = spill.evict(&weights, memory, None, evicted) else {
        return (None, false);
    };
    assert!(sent == expected);
    if let Some(partition) = sent {
        assert!(word(partition, Field::Flags) & ON_DISK != 0);
    }
    (sent, pressed)
}

/// The rule with the weights of a grouping's spill, as its settings give
/// them by default.
#[kani::proof]
#[kani::unwind(31)]
fn a_grouping_sends_what_the_rule_says() {
    let (sent, pressed) = the_rule_sends_what_its_documentation_says(Weights {
        start: 0.875,
        target: 0.5,
        spilled: 1.0,
        reserve: 0,
        resident: 0.0,
        per_check: 0,
    });
    kani::cover!(sent.is_some() && pressed, "the largest goes");
}

/// The rule with the weights of a join's spill, as its settings give them
/// by default, any reserve for the tails of the partitions on disk and
/// any limit a check; with no share of the rows in memory, as a shared
/// join's spill takes it; and with a share above one, which only a caller
/// of the crate may give, so that the partition on disk the rule of the
/// records asks for counts.
#[kani::proof]
#[kani::unwind(31)]
fn a_join_sends_what_the_rule_says() {
    let (sent, pressed) = the_rule_sends_what_its_documentation_says(Weights {
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        reserve: kani::any(),
        resident: match kani::any::<u8>() {
            0 => 0.0,
            1 => 2.0,
            _ => 0.25,
        },
        per_check: kani::any(),
    });
    kani::cover!(sent.is_some() && !pressed, "the lowest in memory goes");
    kani::cover!(sent.is_some() && pressed, "the largest goes");
}
