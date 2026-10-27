//! The matrix of wrong arguments: each rule of a wrong call broken by one
//! argument of one call over a valid table. The call fails with a wrong
//! call and that rule, takes no memory, and, when the argument is a size,
//! a count or a block, which a call checks before its first row, leaves
//! every byte of the blocks as it was.

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::world::*;
use super::*;
use crate::table::testing::{Column, Failing, Keys};

/// What a wrong argument leaves.
#[derive(Clone, Copy, Debug)]
enum Leaves {
    /// Every byte of the blocks as it was: a size, a count or a block,
    /// checked before the first row.
    Unchanged,
    /// What the rows before the wrong one changed.
    Rows,
    /// What the entry checks itself, past a change of its own that sets
    /// the case up.
    Checked,
}

/// One argument of one call broken, by its name, the rule it breaks and
/// what it leaves.
type Wrong = (
    &'static str,
    Rule,
    Leaves,
    fn(&mut World) -> Result<(), Error>,
);

/// A mask of 70 rows over `words`.
fn mask70(words: &mut [u64]) -> Result<RowMask<'_>, Error> {
    RowMask::try_new(70, words)
}

/// Probe every row with `keys`, and hand the table and the matches to
/// `then`.
fn probed<K: KeySource + ?Sized>(
    world: &World,
    keys: &K,
    then: impl FnOnce(&Table<'_>, &Matches<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let nrows = world.nrows();
    let (mut rows, mut refs) = (all_rows(nrows), vec![0; nrows]);
    let table = quiet(|| world.table())?;
    let mut matches = Matches::new(&mut rows, &mut refs)?;
    quiet(|| table.probe(&world.hashes, keys, &mut matches))?;
    then(&table, &matches)
}

/// Every rule of a wrong call over blocks a caller gives, each broken
/// once; [`every_rule_has_its_case`] names those left out.
const WRONGS: &[Wrong] = &[
    (
        "a mask of too few words",
        Rule::MaskWords,
        Leaves::Unchanged,
        |_| quiet(|| RowMaskView::try_new(70, &[0])).map(drop),
    ),
    (
        "a mask with a bit past its rows",
        Rule::MaskPadding,
        Leaves::Unchanged,
        |_| {
            let mut words = [0, 1 << 6];
            quiet(|| mask70(&mut words)).map(drop)
        },
    ),
    ("a row past the mask", Rule::Row, Leaves::Unchanged, |_| {
        let mask = RowMaskView::try_new(70, &[0, 0])?;
        quiet(|| mask.contains(70)).map(drop)
    }),
    (
        "a word past the mask",
        Rule::MaskWord,
        Leaves::Unchanged,
        |_| {
            let mut words = [0, 0];
            let mut mask = mask70(&mut words)?;
            quiet(|| mask.set_word(2, 0))
        },
    ),
    (
        "a word with a bit past the rows",
        Rule::WordPadding,
        Leaves::Unchanged,
        |_| {
            let mut words = [0, 0];
            let mut mask = mask70(&mut words)?;
            quiet(|| mask.set_word(1, u64::MAX))
        },
    ),
    (
        "masks of other rows",
        Rule::MaskRows,
        Leaves::Unchanged,
        |_| {
            let mut words = [0, 0];
            let mut mask = mask70(&mut words)?;
            let other = RowMaskView::try_new(64, &[0])?;
            quiet(|| mask.intersect(other))
        },
    ),
    (
        "hashes of other rows",
        Rule::RowCounts,
        Leaves::Unchanged,
        |world| {
            let nrows = world.nrows();
            let (mut rows, mut refs) = (all_rows(nrows), vec![0; nrows]);
            let table = quiet(|| world.table())?;
            let mut matches = Matches::new(&mut rows, &mut refs)?;
            let hashes = &world.hashes[..nrows - 1];
            quiet(|| table.probe(hashes, &world.keys(), &mut matches)).map(drop)
        },
    ),
    (
        "an index not aligned",
        Rule::IndexAlign,
        Leaves::Unchanged,
        |world| {
            // SAFETY: a byte of the index, which the call refuses unread.
            let index = unsafe { world.index.base().add(4) };
            // SAFETY: the blocks as `World::table` gives them.
            quiet(|| unsafe { Table::attach(index, world.index.len - 8, world.chunks()) }).map(drop)
        },
    ),
    (
        "an index shorter than its header",
        Rule::IndexShort,
        Leaves::Unchanged,
        |world| {
            // SAFETY: as `World::table`, over fewer of the index's bytes.
            quiet(|| unsafe { Table::attach(world.index.base(), 16, world.chunks()) }).map(drop)
        },
    ),
    (
        "a new index of an odd length",
        Rule::IndexLen,
        Leaves::Unchanged,
        |world| create_in(world, |len| len - 4),
    ),
    (
        "a new index too small",
        Rule::IndexSmall,
        Leaves::Unchanged,
        |world| create_in(world, |len| len - 8),
    ),
    (
        "a larger index too small for its buckets",
        Rule::IndexBuckets,
        Leaves::Unchanged,
        |world| {
            let capacity = 2 * world.capacity;
            let grown = Guarded::new(index_size(&world.config, capacity)?);
            let mut table = quiet(|| world.table_mut())?;
            // SAFETY: the new index is ours alone.
            quiet(|| unsafe { table.regrow(grown.base(), grown.len - 8, capacity) })
        },
    ),
    (
        "a larger index of fewer buckets",
        Rule::IndexShrinks,
        Leaves::Checked,
        |world| {
            // Past the least buckets first, so that fewer are possible.
            let capacity = 4096;
            let grown = Guarded::new(index_size(&world.config, capacity)?);
            regrow_into(world, grown, capacity)?;
            let before = world.bytes();
            let fewer = Guarded::new(index_size(&world.config, 1)?);
            let mut table = quiet(|| world.table_mut())?;
            // SAFETY: the new index is ours alone.
            let shrunk = quiet(|| unsafe { table.regrow(fewer.base(), fewer.len, 1) });
            assert!(
                world.bytes() == before,
                "a refused regrow changed the blocks"
            );
            shrunk
        },
    ),
    (
        "a record past a chunk",
        Rule::RecordFit,
        Leaves::Unchanged,
        |_| {
            let config = TableConfig {
                nkeys: 1,
                payload_size: MAX_CHUNK_LEN,
            };
            quiet(|| record_bytes(&config)).map(drop)
        },
    ),
    ("no key", Rule::KeyCount, Leaves::Unchanged, |_| {
        let config = TableConfig {
            nkeys: 0,
            payload_size: 8,
        };
        quiet(|| record_bytes(&config)).map(drop)
    }),
    (
        "a record size no table has",
        Rule::RecordBytes,
        Leaves::Unchanged,
        |world| quiet(|| mark_words(world.lens[0], 12)).map(drop),
    ),
    (
        "another configuration",
        Rule::Config,
        Leaves::Unchanged,
        |world| {
            let table = quiet(|| world.table())?;
            let config = TableConfig {
                nkeys: world.config.nkeys,
                payload_size: world.config.payload_size + 8,
            };
            quiet(|| table.check_config(&config))
        },
    ),
    (
        "keys of another count",
        Rule::Keys,
        Leaves::Unchanged,
        |world| {
            let nrows = world.nrows();
            let values = vec![0; nrows];
            let columns: Vec<Column> = (0..=world.config.nkeys)
                .map(|_| Column::new(&values, None))
                .collect();
            let (mut rows, mut refs) = (all_rows(nrows), vec![0; nrows]);
            let table = quiet(|| world.table())?;
            let mut matches = Matches::new(&mut rows, &mut refs)?;
            quiet(|| table.probe(&world.hashes, &Keys(&columns), &mut matches)).map(drop)
        },
    ),
    (
        "a key past the keys",
        Rule::Key,
        Leaves::Unchanged,
        |world| {
            let nkeys = world.config.nkeys;
            quiet(|| world.table_mut()?.clear_key(nkeys)).map(drop)
        },
    ),
    (
        "keys that cannot be read",
        Rule::KeyRead,
        Leaves::Rows,
        |world| {
            let keys = Failing(&world.keys(), 0);
            let (mut rows, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
            let table = quiet(|| world.table())?;
            let mut matches = Matches::new(&mut rows, &mut refs)?;
            quiet(|| table.probe(&world.hashes, &keys, &mut matches)).map(drop)
        },
    ),
    (
        "a payload of another length",
        Rule::PayloadBytes,
        Leaves::Unchanged,
        |world| {
            let chunk = world.spare(WORK);
            let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
            let mut table = quiet(|| world.table_mut())?;
            let keys = world.keys();
            let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
            let payload = &world.payload[..world.payload.len() - 1];
            quiet(|| table.append(chunk, Some(payload), &mut batch)).map(drop)
        },
    ),
    (
        "columns that do not fill the payload",
        Rule::PayloadColumns,
        Leaves::Unchanged,
        |world| {
            let chunk = world.spare(WORK);
            let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
            let mut table = quiet(|| world.table_mut())?;
            let keys = world.keys();
            let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
            two_columns(world, |columns| {
                quiet(|| table.append_columns(chunk, columns, &mut batch)).map(drop)
            })
        },
    ),
    (
        "columns that do not fill the payload of partitions",
        Rule::PartitionColumns,
        Leaves::Unchanged,
        |world| {
            let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
            let writer = quiet(|| writer(&world.config, world.chunks()))?;
            let keys = world.keys();
            let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
            let chunks = WRITER_PARTS.map(|part| world.spare(part) as u32);
            let (mut counts, mut nulls) = ([0; 2], 0);
            two_columns(world, |columns| {
                quiet(|| {
                    writer.append_partitioned_columns(
                        &Partitions {
                            shift: SHIFT,
                            chunks: &chunks,
                        },
                        columns,
                        &mut batch,
                        &mut Appended {
                            rows: &mut counts,
                            nulls: &mut nulls,
                        },
                    )
                })
                .map(drop)
            })
        },
    ),
    (
        "too many columns",
        Rule::PayloadShape,
        Leaves::Unchanged,
        |world| {
            let (value, null) = (vec![0; world.nrows()], vec![false; world.nrows()]);
            let values = vec![&value[..]; MAX_PAYLOAD_COLUMNS + 1];
            let nulls = vec![&null[..]; MAX_PAYLOAD_COLUMNS + 1];
            quiet(|| PayloadColumns::new(&values, &nulls, world.nrows())).map(drop)
        },
    ),
    (
        "a column of other rows",
        Rule::PayloadRows,
        Leaves::Unchanged,
        |world| {
            let (value, null) = (vec![0; world.nrows() + 1], vec![false; world.nrows()]);
            let (values, nulls) = ([&value[..]], [&null[..]]);
            quiet(|| PayloadColumns::new(&values, &nulls, world.nrows())).map(drop)
        },
    ),
    (
        "a payload word off a word",
        Rule::PayloadWord,
        Leaves::Unchanged,
        |world| {
            let mut out = vec![0; world.nrows()];
            probed(world, &world.keys(), |table, matches| {
                quiet(|| table.gather(matches, 3, &mut out))
            })
        },
    ),
    (
        "payload words past the payload",
        Rule::PayloadWords,
        Leaves::Unchanged,
        |world| {
            let mut out = vec![0; world.nrows()];
            probed(world, &world.keys(), |table, matches| {
                quiet(|| table.gather_words(matches, 2, &mut [&mut out[..]]))
            })
        },
    ),
    (
        "an output of other rows",
        Rule::BatchRows,
        Leaves::Unchanged,
        |world| {
            let mut out = vec![0; world.nrows() + 1];
            probed(world, &world.keys(), |table, matches| {
                quiet(|| table.gather_words(matches, 0, &mut [&mut out[..]]))
            })
        },
    ),
    (
        "a chunk past the chunks",
        Rule::NoChunk,
        Leaves::Unchanged,
        |world| {
            let chunk = world.lens.len();
            let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
            let mut table = quiet(|| world.table_mut())?;
            let keys = world.keys();
            let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
            quiet(|| table.append(chunk, Some(&world.payload), &mut batch)).map(drop)
        },
    ),
    (
        "a chunk of a length no chunk has",
        Rule::Chunk,
        Leaves::Unchanged,
        |world| {
            let mut lens = world.lens.clone();
            lens[0] -= 4;
            // SAFETY: the world's blocks, the first given shorter.
            let chunks = quiet(|| unsafe { Chunks::new(&world.bases, &lens) })?;
            quiet(|| chunks.check_all())
        },
    ),
    (
        "a block of a length no chunk has",
        Rule::Block,
        Leaves::Unchanged,
        |world| {
            let (base, len) = (world.bases[0], world.lens[0] - 4);
            // SAFETY: the first chunk's block, given shorter.
            quiet(|| unsafe { Chunks::used(base, len, world.record_size) }).map(drop)
        },
    ),
    (
        "bases and lengths that do not pair up",
        Rule::Chunks,
        Leaves::Unchanged,
        |world| {
            let lens = &world.lens[..world.lens.len() - 1];
            // SAFETY: the world's blocks, which the call refuses unread.
            quiet(|| unsafe { Chunks::new(&world.bases, lens) }).map(drop)
        },
    ),
    (
        "too many chunks",
        Rule::Chunks,
        Leaves::Unchanged,
        |world| {
            let bases = vec![world.bases[0]; MAX_CHUNKS + 1];
            let lens = vec![world.lens[0]; MAX_CHUNKS + 1];
            // SAFETY: the first chunk's block, which the call refuses unread.
            quiet(|| unsafe { Chunks::new(&bases, &lens) }).map(drop)
        },
    ),
    (
        "a link from no record's start",
        Rule::Boundary,
        Leaves::Unchanged,
        |world| {
            let chunk = world.spare(WORK);
            let mut from = CHUNK_HEADER + 1;
            quiet(|| world.table_mut()?.link(chunk, &mut from)).map(drop)
        },
    ),
    (
        "a mark of no record",
        Rule::Reference,
        Leaves::Rows,
        |world| {
            let table = quiet(|| world.table())?;
            let refs = [u32::MAX];
            let rows = RowMaskView::try_new(1, &[1])?;
            quiet(|| table.mark(&refs, &rows, &MarkBlocks(&world.marks)))
        },
    ),
    (
        "a cursor past the chunks",
        Rule::Cursor,
        Leaves::Unchanged,
        |world| {
            let table = quiet(|| world.table_mut())?;
            let mut cursor = Cursor::at(world.lens.len() + 1, CHUNK_HEADER);
            quiet(|| table.scan(&mut cursor, &mut [0; 7])).map(drop)
        },
    ),
    (
        "a cursor past its chunk's records",
        Rule::CursorPast,
        Leaves::Unchanged,
        |world| {
            let table = quiet(|| world.table_mut())?;
            let used = world.used(0);
            let mut cursor = Cursor::at(0, used + world.record_size);
            quiet(|| table.scan(&mut cursor, &mut [0; 7])).map(drop)
        },
    ),
    (
        "a walk into no room",
        Rule::EmptyWalk,
        Leaves::Unchanged,
        |world| {
            let table = quiet(|| world.table_mut())?;
            quiet(|| table.scan(&mut Cursor::start(), &mut [])).map(drop)
        },
    ),
    (
        "three partitions",
        Rule::Partitions,
        Leaves::Unchanged,
        |world| partitioned(world, SHIFT, &[0, 1, 2]),
    ),
    (
        "partitions past the hash",
        Rule::PartitionBits,
        Leaves::Unchanged,
        |world| partitioned(world, 32, &[0, 1]),
    ),
    (
        "counts of other partitions",
        Rule::PartitionRows,
        Leaves::Unchanged,
        |world| appended_by_partition(world, &mut [0; 3]),
    ),
    (
        "counts at their most",
        Rule::PartitionCount,
        Leaves::Rows,
        |world| appended_by_partition(world, &mut [u64::MAX; 2]),
    ),
    (
        "references and hashes of other lengths",
        Rule::SplitLengths,
        Leaves::Unchanged,
        |world| {
            split(
                world,
                &mut [0; 4],
                &mut [0; 3],
                SPLIT_PARTS.map(|part| world.spare(part)),
            )
        },
    ),
    (
        "a split into no room",
        Rule::EmptySplit,
        Leaves::Unchanged,
        |world| {
            split(
                world,
                &mut [],
                &mut [],
                SPLIT_PARTS.map(|part| world.spare(part)),
            )
        },
    ),
    (
        "a split into itself",
        Rule::SplitIntoItself,
        Leaves::Unchanged,
        |world| {
            split(
                world,
                &mut [0; 4],
                &mut [0; 4],
                [0, world.spare(SPLIT_PARTS[1])],
            )
        },
    ),
    (
        "a merge into itself",
        Rule::MergeIntoItself,
        Leaves::Unchanged,
        |world| {
            let source = world.spare(SOURCE);
            let mut from = CHUNK_HEADER;
            quiet(|| {
                world
                    .table_mut()?
                    .merge(source, &mut from, source, |_, _| Ok::<_, Error>(()))
            })
            .map(drop)
        },
    ),
    (
        "a slot past the held",
        Rule::Slot,
        Leaves::Unchanged,
        |world| {
            let reference = world.refs[0];
            let mut table = quiet(|| world.table_mut())?;
            quiet(|| table.payloads().hold(HELD, reference))
        },
    ),
    (
        "a slot that holds nothing",
        Rule::EmptySlot,
        Leaves::Unchanged,
        |world| {
            let mut table = quiet(|| world.table_mut())?;
            quiet(|| table.payloads().held(0).map(drop))
        },
    ),
    (
        "a filter not aligned",
        Rule::FilterAlign,
        Leaves::Unchanged,
        |world| {
            // SAFETY: a byte of the filter's block, which the call refuses
            // unread.
            let words = unsafe { world.shared.base().add(4) }.cast();
            // SAFETY: as above.
            quiet(|| unsafe { bloom::SharedFilter::attach(words, 2) }).map(drop)
        },
    ),
    (
        "a shared filter of four words",
        Rule::FilterWords,
        Leaves::Unchanged,
        |world| {
            let words = world.shared.base().cast();
            // SAFETY: the shared filter's words, which the call refuses
            // unread; its state and three words, not a power of two.
            quiet(|| unsafe { bloom::SharedFilter::attach(words, 4) }).map(drop)
        },
    ),
    (
        "a filter for too many records",
        Rule::FilterRecords,
        Leaves::Unchanged,
        |_| quiet(|| bloom::words_for(u64::MAX)).map(drop),
    ),
    (
        "a filter of three words to probe",
        Rule::FilterSize,
        Leaves::Unchanged,
        |world| {
            let mut rows = all_rows(world.nrows());
            let mut rows = RowMask::try_new(world.nrows(), &mut rows)?;
            quiet(|| bloom::probe(&[0; 3], &world.hashes, &mut rows)).map(drop)
        },
    ),
    (
        "a filter to fill not aligned",
        Rule::FilterFillAlign,
        Leaves::Unchanged,
        |world| {
            let rows = all_rows(world.nrows());
            let rows = RowMaskView::try_new(world.nrows(), &rows)?;
            // SAFETY: a byte of the filter's block, which the call refuses
            // unwritten.
            let words = unsafe { world.filter.base().add(4) }.cast();
            // SAFETY: as above.
            quiet(|| unsafe { bloom::add_atomic(words, 1, &world.hashes, &rows) })
        },
    ),
    (
        "a shared filter not built",
        Rule::FilterNotBuilt,
        Leaves::Unchanged,
        |world| {
            let nwords = world.shared.len / 8;
            // SAFETY: the block's words, reached only through this filter.
            let filter =
                unsafe { bloom::SharedFilter::attach(world.shared.base().cast(), nwords) }?;
            quiet(|| filter.init());
            let snapshot = world.bytes();
            let mut rows = all_rows(world.nrows());
            let mut rows = RowMask::try_new(world.nrows(), &mut rows)?;
            let probed = quiet(|| bloom::probe_shared(&filter, &world.hashes, &mut rows));
            assert!(
                world.bytes() == snapshot,
                "a refused probe changed the filter"
            );
            probed.map(drop)
        },
    ),
    (
        "a reference outside the records",
        Rule::Outside,
        Leaves::Unchanged,
        |world| {
            let table = quiet(|| world.table())?;
            let reference = reference(world.lens.len(), CHUNK_HEADER);
            quiet(|| table.record(reference)).map(drop)
        },
    ),
];

/// Create a table in a new index of the length `len` makes of the one
/// its capacity needs.
fn create_in(world: &World, len: fn(usize) -> usize) -> Result<(), Error> {
    let grown = Guarded::new(index_size(&world.config, world.capacity)?);
    let config = world.config;
    // SAFETY: the new index is ours alone; the chunks as the world's.
    quiet(|| unsafe {
        TableMut::create(
            grown.base(),
            len(grown.len),
            &config,
            world.capacity,
            world.chunks(),
        )
    })
    .map(drop)
}

/// Call `call` with two payload columns of the rows.
fn two_columns(
    world: &World,
    call: impl FnOnce(&PayloadColumns<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let (value, null) = (vec![0; world.nrows()], vec![false; world.nrows()]);
    let (values, nulls) = ([&value[..], &value[..]], [&null[..], &null[..]]);
    call(&PayloadColumns::new(&values, &nulls, world.nrows())?)
}

/// Find or insert every row by partitions of `shift` over the spare
/// chunks numbered `parts`.
fn partitioned(world: &mut World, shift: u32, parts: &[usize]) -> Result<(), Error> {
    let nrows = world.nrows();
    let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
    let mut inserted = vec![0; nrows.div_ceil(64)];
    let chunks: Vec<u32> = parts
        .iter()
        .map(|&part| world.spare(PARTS[0] + part) as u32)
        .collect();
    let mut table = quiet(|| world.table_mut())?;
    let keys = world.keys();
    let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
    let mut inserted = RowMask::try_new(nrows, &mut inserted)?;
    let partitions = Partitions {
        shift,
        chunks: &chunks,
    };
    quiet(|| table.find_or_insert_partitioned(&partitions, &mut batch, &mut inserted)).map(drop)
}

/// Append every row by partition, the count of each in `counts`.
fn appended_by_partition(world: &mut World, counts: &mut [u64]) -> Result<(), Error> {
    let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
    let writer = quiet(|| writer(&world.config, world.chunks()))?;
    let keys = world.keys();
    let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
    let chunks = WRITER_PARTS.map(|part| world.spare(part) as u32);
    let mut nulls = 0;
    world.with_columns(|columns| {
        quiet(|| {
            writer.append_partitioned_columns(
                &Partitions {
                    shift: SHIFT,
                    chunks: &chunks,
                },
                columns,
                &mut batch,
                &mut Appended {
                    rows: counts,
                    nulls: &mut nulls,
                },
            )
        })
        .map(drop)
    })
}

/// Split the first chunk into the chunks `into`, with outputs `copies`
/// and `hashes`.
fn split(
    world: &World,
    copies: &mut [u32],
    hashes: &mut [u32],
    into: [usize; 2],
) -> Result<(), Error> {
    let writer = quiet(|| writer(&world.config, world.chunks()))?;
    let chunks = into.map(|chunk| chunk as u32);
    let partitions = Partitions {
        shift: SHIFT,
        chunks: &chunks,
    };
    let mut from = CHUNK_HEADER;
    quiet(|| writer.split(&partitions, 0, &mut from, copies, hashes)).map(drop)
}

/// Each rule of a wrong call broken by one argument of one call: the call
/// fails with a wrong call and that rule, takes no memory, and leaves the
/// blocks as they were when the argument is checked before the first row.
#[test]
fn each_wrong_argument_names_its_rule() {
    let config = Config {
        cases: if cfg!(miri) { 1 } else { 8 },
        failure_persistence: None,
        ..Config::default()
    };
    let outcome = TestRunner::new(config).run(&cases(), |case| {
        for &(what, rule, leaves, call) in WRONGS {
            let mut world = World::build(&case);
            let before = world.bytes();
            let result = call(&mut world);
            prop_assert_eq!(refusal(result), Some((Kind::WrongCall, rule)), "{}", what);
            if let Leaves::Unchanged = leaves {
                prop_assert!(world.bytes() == before, "{what}: the blocks changed");
            }
            prop_assert!(world.guarded(), "{what}: a write past a block");
        }
        Ok(())
    });
    if let Err(failure) = outcome {
        panic!("{failure}");
    }
}

/// Every rule of the crate has its case: a wrong argument of the matrix
/// here, a damage of the matrix of damage, or a test of its own, named
/// with its reason.
#[test]
fn every_rule_has_its_case() {
    let elsewhere = [
        // A filter of fewer than two words fails FilterWords first.
        (Rule::FilterState, "no call can break it"),
        (Rule::RecordRoom, "the owning table's: tests/owned.rs"),
        (Rule::MostRecords, "the owning table's: tests/owned.rs"),
        (
            Rule::Grew,
            "damage.rs: a_header_changed_under_a_handle_fails_its_regrow",
        ),
        (Rule::ChunkLimit, "the owning table's: tests/format.rs"),
        (
            Rule::MergedFound,
            "tests.rs: a_merge_refuses_a_record_found_in_its_source",
        ),
    ];
    let wrong: Vec<Rule> = WRONGS.iter().map(|&(_, rule, _, _)| rule).collect();
    let damaged: Vec<Rule> = super::damage::DAMAGES
        .iter()
        .flat_map(|&(_, _, expect)| match expect {
            super::damage::Expect::Data => &[][..],
            super::damage::Expect::Seen(rules) | super::damage::Expect::Maybe(rules) => rules,
        })
        .copied()
        .collect();
    for &rule in crate::error::rules() {
        assert!(
            wrong.contains(&rule)
                || damaged.contains(&rule)
                || elsewhere.iter().any(|&(other, _)| other == rule),
            "{rule:?} has no case"
        );
    }
}
