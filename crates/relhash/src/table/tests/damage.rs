//! The matrix of damage: valid tables of four shapes, each damaged in one
//! place from a closed list, every field at and around its limits, and
//! every call of the crate run over it twice, so that calls also follow
//! the ones that failed. Each call ends with a result or the error of
//! the rule the damage breaks, a damage that is more than data is seen by
//! some call, and the words around every block stay as they were.

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::world::*;
use super::*;

/// What a damage leads the calls to.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Only data: every call succeeds.
    Data,
    /// A call that fails gives a damaged table and one of these rules,
    /// and some call fails.
    Seen(&'static [Rule]),
    /// As `Seen`, but no call need fail: the damage may lie where no walk
    /// of this table goes.
    Maybe(&'static [Rule]),
}

/// One place damaged in one way, by its name.
type Damage = (&'static str, fn(&World), Expect);

/// Add `delta` to the `u32` at `byte` of the index.
fn bump32(world: &World, byte: usize, delta: i32) {
    let value = world.index.read::<u32>(byte);
    world.index.write(byte, value.wrapping_add_signed(delta));
}

/// Add `delta` to the `u64` at `byte` of the index.
fn bump64(world: &World, byte: usize, delta: i64) {
    let value = world.index.read::<u64>(byte);
    world.index.write(byte, value.wrapping_add_signed(delta));
}

/// The closed list of damage: the fields of the header, the count of
/// records, a bucket, the fields of a record and a used mark, each at and
/// around its limits.
const DAMAGES: &[Damage] = &[
    ("nothing", |_| {}, Expect::Data),
    // The header.
    (
        "magic",
        |w| bump64(w, MAGIC, 1),
        Expect::Seen(&[Rule::Magic]),
    ),
    (
        "version above",
        |w| bump32(w, VERSION, 1),
        Expect::Seen(&[Rule::Version]),
    ),
    (
        "version below",
        |w| bump32(w, VERSION, -1),
        Expect::Seen(&[Rule::Version]),
    ),
    (
        "header size above",
        |w| bump32(w, HEADER_SIZE, 8),
        Expect::Seen(&[Rule::HeaderSize]),
    ),
    (
        "header size below",
        |w| bump32(w, HEADER_SIZE, -8),
        Expect::Seen(&[Rule::HeaderSize]),
    ),
    (
        "index longer",
        |w| bump64(w, REGION_LEN, 8),
        Expect::Seen(&[Rule::IndexLength]),
    ),
    (
        "index shorter",
        |w| bump64(w, REGION_LEN, -8),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "index of odd length",
        |w| bump64(w, REGION_LEN, -4),
        Expect::Seen(&[Rule::IndexShape]),
    ),
    (
        "index of no length",
        |w| w.index.write(REGION_LEN, 0_u64),
        Expect::Seen(&[Rule::IndexShape]),
    ),
    (
        "index of most length",
        |w| w.index.write(REGION_LEN, u64::MAX),
        Expect::Seen(&[Rule::IndexLength]),
    ),
    (
        "buckets later",
        |w| bump64(w, BUCKETS_OFFSET, 8),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "buckets misaligned",
        |w| bump64(w, BUCKETS_OFFSET, -4),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "buckets at the start",
        |w| w.index.write(BUCKETS_OFFSET, 0_u64),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "reserved word used",
        |w| w.index.write(RESERVED_USED, 1_u64),
        Expect::Seen(&[Rule::Reserved]),
    ),
    (
        "twice the buckets",
        |w| bump32(w, NBUCKETS, w.index.read::<u32>(NBUCKETS) as i32),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "half the buckets",
        |w| bump32(w, NBUCKETS, -(w.index.read::<u32>(NBUCKETS) as i32 / 2)),
        // Fewer than the least buckets, or a count the shift does not give.
        Expect::Seen(&[Rule::BucketCount, Rule::BucketShift]),
    ),
    (
        "a bucket less",
        |w| bump32(w, NBUCKETS, -1),
        Expect::Seen(&[Rule::BucketCount]),
    ),
    (
        "no bucket",
        |w| w.index.write(NBUCKETS, 0_u32),
        Expect::Seen(&[Rule::BucketCount]),
    ),
    (
        "most buckets",
        |w| w.index.write(NBUCKETS, u32::MAX),
        Expect::Seen(&[Rule::BucketCount]),
    ),
    (
        "shift above",
        |w| bump32(w, BUCKET_SHIFT, 1),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "shift below",
        |w| bump32(w, BUCKET_SHIFT, -1),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "shift of the hash",
        |w| w.index.write(BUCKET_SHIFT, 32_u32),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "record size above",
        |w| bump32(w, RECORD_SIZE, 8),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "record size below",
        |w| bump32(w, RECORD_SIZE, -8),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "record size zero",
        |w| w.index.write(RECORD_SIZE, 0_u32),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "record size most",
        |w| w.index.write(RECORD_SIZE, u32::MAX),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "payload above",
        |w| bump32(w, PAYLOAD_SIZE, 1),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "payload most",
        |w| w.index.write(PAYLOAD_SIZE, u32::MAX),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "no key",
        |w| w.index.write(NKEYS, 0_u32),
        Expect::Seen(&[Rule::KeyCount]),
    ),
    (
        "a key more",
        |w| bump32(w, NKEYS, 1),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "too many keys",
        |w| w.index.write(NKEYS, MAX_KEYS as u32 + 1),
        Expect::Seen(&[Rule::KeyCount]),
    ),
    (
        "a flag",
        |w| w.index.write(FLAGS, 1_u32),
        Expect::Seen(&[Rule::Flags]),
    ),
    (
        "every flag",
        |w| w.index.write(FLAGS, u32::MAX),
        Expect::Seen(&[Rule::Flags]),
    ),
    (
        "first reserved word",
        |w| w.index.write(RESERVED, 1_u64),
        Expect::Seen(&[Rule::Reserved]),
    ),
    (
        "last reserved word",
        |w| w.index.write(RESERVED + 16, u64::MAX),
        Expect::Seen(&[Rule::Reserved]),
    ),
    // The count of records.
    (
        "no record",
        |w| w.index.write(NRECORDS, 0_u64),
        Expect::Seen(&[Rule::Chain]),
    ),
    (
        "one record",
        |w| w.index.write(NRECORDS, 1_u64),
        Expect::Maybe(&[Rule::Chain]),
    ),
    (
        "a record less",
        |w| bump64(w, NRECORDS, -1),
        Expect::Maybe(&[Rule::Chain]),
    ),
    ("a record more", |w| bump64(w, NRECORDS, 1), Expect::Data),
    (
        "most records",
        |w| w.index.write(NRECORDS, u64::MAX),
        // An append adds to the count, which wraps to a few records, and
        // a walk past them fails as a cycle.
        Expect::Maybe(&[Rule::Chain]),
    ),
    // The bucket of the victim's hash.
    (
        "bucket emptied",
        |w| w.index.write(w.victim_bucket(), 0_u32),
        Expect::Data,
    ),
    (
        "bucket to a missing chunk",
        |w| {
            w.index
                .write(w.victim_bucket(), reference(w.lens.len(), CHUNK_HEADER))
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket to a used mark",
        |w| w.index.write(w.victim_bucket(), reference(1, 0)),
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket past its chunk",
        |w| {
            let chunk = placement(w.victim).0;
            w.index
                .write(w.victim_bucket(), reference(chunk, w.lens[chunk] - 8))
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket a unit past the last record",
        |w| {
            let chunk = placement(w.victim).0;
            let byte = w.lens[chunk] - w.record_size + 8;
            w.index.write(w.victim_bucket(), reference(chunk, byte))
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket to the last record",
        |w| {
            let chunk = placement(w.victim).0;
            let byte = w.lens[chunk] - w.record_size;
            w.index.write(w.victim_bucket(), reference(chunk, byte))
        },
        // A record of another chain when the chunk is full, else zeros.
        Expect::Maybe(&[Rule::Misplaced]),
    ),
    (
        "bucket past the used mark",
        |w| {
            let chunk = placement(w.victim).0;
            w.index
                .write(w.victim_bucket(), reference(chunk, w.used(chunk)))
        },
        Expect::Seen(&[Rule::Misplaced, Rule::Outside]),
    ),
    (
        "bucket inside a record",
        |w| w.index.write(w.victim_bucket(), w.victim + 1),
        Expect::Seen(&[Rule::Misplaced, Rule::Outside]),
    ),
    (
        "bucket of most",
        |w| w.index.write(w.victim_bucket(), u32::MAX),
        Expect::Seen(&[Rule::Outside]),
    ),
    // The victim's record.
    (
        "next to itself",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, w.victim);
        },
        Expect::Seen(&[Rule::Chain]),
    ),
    (
        "next to a missing chunk",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, reference(w.lens.len(), CHUNK_HEADER));
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "next past its chunk",
        |w| {
            let (chunk, byte) = placement(w.victim);
            w.blocks[chunk].write(byte + 4, reference(chunk, w.lens[chunk] - 8));
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "next a unit past the last record",
        |w| {
            let (chunk, byte) = placement(w.victim);
            let last = w.lens[chunk] - w.record_size + 8;
            w.blocks[chunk].write(byte + 4, reference(chunk, last));
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "next inside a record",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, w.victim + 1);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::Outside]),
    ),
    (
        "next of most",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, u32::MAX);
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "hash",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte, block.read::<u32>(byte) ^ 1);
        },
        Expect::Data,
    ),
    (
        "NULL bits",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 8, u32::MAX);
        },
        Expect::Data,
    ),
    (
        "record size field above",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 12, block.read::<u32>(byte + 12) + 1);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::NoRecord]),
    ),
    (
        "record size field zero",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 12, 0_u32);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::NoRecord]),
    ),
    (
        "record size field most",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 12, u32::MAX);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::NoRecord]),
    ),
    (
        "key",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 16, block.read::<u64>(byte + 16) ^ 1);
        },
        Expect::Data,
    ),
    (
        "payload",
        |w| {
            let (block, byte) = w.victim();
            let at = byte + w.record_size - 8;
            block.write(at, !block.read::<u64>(at));
        },
        Expect::Data,
    ),
    // The used mark of the victim's chunk.
    (
        "used mark zero",
        |w| {
            w.victim().0.write(0, 0_u64);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark within itself",
        |w| {
            w.victim().0.write(0, 7_u64);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark off a record",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim().0.write(0, w.used(chunk) as u64 - 8);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark past the chunk",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim().0.write(0, w.lens[chunk] as u64 + 8);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark a record past the chunk",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim()
                .0
                .write(0, (w.lens[chunk] + w.record_size) as u64);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark of most",
        |w| {
            w.victim().0.write(0, u64::MAX);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark a record less",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim()
                .0
                .write(0, (w.used(chunk) - w.record_size) as u64);
        },
        Expect::Data,
    ),
    (
        "used mark at the chunk's end",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim().0.write(0, w.lens[chunk] as u64);
        },
        Expect::Maybe(&[Rule::NoRecord]),
    ),
];

/// Run `run`, a call named `call`, over `world`, damaged by `damage`:
/// its outcome must be what the damage leads to, and every block's guard
/// words as they were. Note in `seen` whether it saw the damage.
fn judge(
    world: &mut World,
    (call, run): Call,
    (damage, expect): (&str, Expect),
    pass: &str,
    seen: &mut bool,
) -> Result<(), TestCaseError> {
    let result = run(world);
    prop_assert!(
        world.guarded(),
        "{damage}: {call} ({pass}) wrote past a block"
    );
    let Err(error) = result else {
        return Ok(());
    };
    let rule = (error.kind(), error.rule());
    match expect {
        Expect::Data => prop_assert!(false, "{damage}: {call} ({pass}) failed: {rule:?}"),
        Expect::Seen(rules) | Expect::Maybe(rules) => prop_assert!(
            rule.0 == Kind::Damaged && rules.contains(&rule.1),
            "{damage}: {call} ({pass}) failed with {rule:?}, not {rules:?}"
        ),
    }
    *seen = true;
    Ok(())
}

/// Damage a table of `case` in every way of the list, and run every call
/// over it: each call alone over the table just damaged, then all of
/// them in turn, twice, so that calls follow the ones that failed.
fn damage_and_call(case: &Case) -> Result<(), TestCaseError> {
    for (index, &(damage, apply, expect)) in DAMAGES.iter().enumerate() {
        // Under Miri, a few of each kind.
        if cfg!(miri) && index % 6 != 0 {
            continue;
        }
        let mut seen = false;
        for &call in CALLS {
            let mut world = World::build(case);
            apply(&world);
            judge(&mut world, call, (damage, expect), "alone", &mut seen)?;
        }
        let mut world = World::build(case);
        apply(&world);
        for round in ["first round", "second round"] {
            for &call in CALLS {
                judge(&mut world, call, (damage, expect), round, &mut seen)?;
            }
        }
        if let Expect::Seen(_) = expect {
            prop_assert!(seen, "{damage}: no call saw it");
        }
    }
    Ok(())
}

#[test]
fn every_damage_against_every_call() {
    let config = Config {
        cases: if cfg!(miri) { 1 } else { 24 },
        failure_persistence: None,
        ..Config::default()
    };
    let outcome = TestRunner::new(config).run(&cases(), |case| damage_and_call(&case));
    if let Err(failure) = outcome {
        panic!("{failure}");
    }
}
