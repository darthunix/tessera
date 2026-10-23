# relhash

A hash table for the joins and the groupings of relational databases.
A hash join keeps the rows of one side in it and looks the rows of the
other side up by their keys. A grouping keeps a record for each group
and finds it again for every row of the group.

- **Memory the caller gives.** The table lives in blocks of memory that
  its caller owns, and keeps no address inside them. The same bytes
  serve one process, several processes over shared memory, and a file
  on disk.
- **Records never move.** A table grows by a new block for records, or
  by a larger array of buckets over the same records, never by a copy.
- **Batches.** A call puts in, or looks up, a batch of rows at once,
  and allocates no memory.
- **Any bytes.** Bytes that are not a table give an error, never a
  crash or a call that does not end.

relhash is the hash table of [Tessera], batch execution for PostgreSQL,
and depends on nothing of it.

## An example

The owning table, `OwnedTable`, allocates its own blocks and needs no
`unsafe` code. Here three rows with the keys 7, 8 and 7 go in, and two
of them are looked up.

```rust
use relhash::{KeySource, Matches, OwnedTable, RowMaskView, TableConfig, ones};

/// One key of `i32` values with no NULL. The table takes the keys of a
/// batch a word of 64 rows at a time.
struct Keys<'a>(&'a [i32]);

impl KeySource for Keys<'_> {
    fn nkeys(&self) -> usize {
        1
    }

    fn nrows(&self) -> usize {
        self.0.len()
    }

    fn word(&self, _key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        for bit in ones(selected) {
            out[bit] = i64::from(self.0[index * 64 + bit]);
        }
        // The rows whose key is not NULL: all of them.
        Some(selected)
    }
}

fn main() -> Result<(), relhash::Error> {
    // One key and a payload of 8 bytes a record, room for 100 records,
    // in chunks of 4 KiB.
    let config = TableConfig { nkeys: 1, payload_size: 8 };
    let mut table = OwnedTable::new(&config, 100, 4096)?;

    // The caller computes the hashes: equal keys, equal hashes. The high
    // bits of a hash pick its bucket, and a product with a prime near
    // 2^32 divided by the golden ratio spreads close keys over them.
    let keys = Keys(&[7, 8, 7]);
    let hashes = [7u32, 8, 7].map(|key| key.wrapping_mul(0x9e37_79b1));
    // Each record's payload: 1, 2 and 3.
    let payload = [1u64, 2, 3].map(u64::to_ne_bytes).concat();
    // A mask of the rows a call takes, a bit a row: here all three.
    let all = RowMaskView::try_new(3, &[0b111])?;
    let mut refs = [0; 3];
    assert_eq!(table.insert(&hashes, &keys, all, Some(&payload), &mut refs)?, 3);

    // Look up rows 0 and 1. A lookup is a mask of the rows still looking
    // and, for each row, the reference of the record it found.
    let table = table.table()?;
    let mut rows = [0b011];
    let mut found = [0; 3];
    let mut matches = Matches::new(&mut rows, &mut found)?;
    // Both find a record: the key 7 of row 0 has two, the key 8 one.
    assert_eq!(table.probe(&hashes, &keys, &mut matches)?, 2);
    assert_eq!(table.record(matches.refs()[1])?.payload, 2u64.to_ne_bytes());
    // Only row 0 has a next record of its key.
    assert_eq!(table.next_match(&mut matches)?, 1);
    assert_eq!(matches.rows().selected_rows().collect::<Vec<_>>(), [0]);
    Ok(())
}
```

## What the caller promises

A table over the caller's own memory is made by `unsafe` calls, and
their contracts say what the caller promises:

- every block it gives, the index and each chunk, is memory of the
  length it states, aligned to 8 bytes and initialized, and no two
  blocks overlap;
- no other process or thread writes the table while a call runs;
- nothing else reads or writes the chunks of a table that one writer
  has to itself, whatever index names them.

Under that promise, whatever bytes the blocks hold, a call ends after
a number of steps bounded by the lengths of its blocks and its
arguments; it returns a result or an error and does not panic; and it
reads and writes no memory but the blocks and the arguments it is
given. This holds after a call that failed as well. Over bytes that
are not a table a result may be wrong: the promise is only this.
Calls in one process over the same bytes, with the same arguments and
in the same order, leave the same bytes and give the same answers.

## Building

relhash builds for targets whose pointers are 64 bits wide, and on any
other target fails to build with an error that says so.

Build a program that uses relhash with link-time optimization, with
`lto = true` in its release profile. Without it the compiler inlines a
small function of relhash into the program's loops only when the
function is generic or marked `#[inline]`. relhash keeps its hot calls
so, and is fast without LTO, as Tessera builds it; with LTO the rest
can be inlined too.

## Keys from outside

The caller gives the hash, and the table chains the records of the
keys whose hashes pick one bucket. When the keys come from outside,
such as from the users of a database, someone can choose keys that
share a hash, and each lookup of them then walks one long chain. Hash
such keys with a secret seed, as Rust's `RandomState` does, so that
nobody outside knows which keys collide.

## How it is built

The [design] says how the table is built and why, for those who change
it; a caller does not need it.

[Tessera]: https://github.com/darthunix/tessera
[design]: https://github.com/darthunix/tessera/blob/main/openspec/specs/relhash/design.md
