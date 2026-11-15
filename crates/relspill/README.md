# relspill

The spill of the hash joins, groupings and sorts of relational
databases. When a join's hash table, a grouping's groups or a sort's
rows outgrow the memory of a query, some of them go to temporary files
and come back later. relspill decides what goes to disk and when, lays
out the bytes of the files, and checks them when they come back. A join
and a grouping split their rows by bits of the hash into partitions,
some kept in memory and some sent to disk; one such split is a level,
and a partition read back that is still too large splits again into a
level below.

- **No input or output.** The crate opens, reads and writes no file. A
  call that writes returns the bytes to write and where they go; a call
  that reads names the bytes it needs and takes them from its caller.
  The files are the caller's: temporary files of a database, or bytes
  in memory, as in the example below. The caller reports a write it
  could not finish, and the set then takes only a reset; a machine
  takes no such report, and its caller ends the spill instead.
- **No memory allocated.** Every region a call uses comes from its
  caller. A call that needs a larger region says how large, and changes
  nothing; a call that appends rows appends those that fit first, and
  says what the next one needs.
- **State machines.** The decisions of a join's spill, of a grouping's,
  of a sort's runs and of a table that several processes build together
  are machines. The caller steps a machine with what it saw, and each
  step answers what to do next.
- **Any bytes.** Bytes read back that are not what the crate wrote give
  an error, never a crash, a call that does not end, or a read past its
  bounds.

relspill is the spill of [Tessera], batch execution for PostgreSQL, and
depends on nothing of it but its hash table, [relhash].

## An example

A **set** is the file of one level of a spill: the blocks of its
partitions one after another and, at its end, a list of each
partition's blocks. Here the file is a vector of bytes. A writer puts
two chunks of values into partition 1 of a set of two partitions and
one into partition 0, and a reader gives partition 1 back.

```rust
use relspill::set::{
    Block, Config, Entry, Lists, ReadConfig, ReadStep, Reader, Region, Step, Writer,
};
use relspill::{BlockKind, Bounds};

/// The block list of each partition, in regions the caller keeps.
struct Kept(Vec<(Vec<Entry>, u64)>);

impl Lists for Kept {
    fn list(&mut self, partition: usize) -> (&mut [Entry], &mut u64) {
        let (entries, count) = &mut self.0[partition];
        (entries, count)
    }
}

/// The regions a writer works in, and the file.
struct Host {
    buffer: Vec<u8>,
    scratch: Vec<u8>,
    lists: Kept,
    file: Vec<u8>,
}

impl Host {
    /// Do what a step asks: grow a region, or write the ranges into the
    /// file and report them. True when the call is done.
    fn serve(
        &mut self,
        writer: &mut Writer,
        step: Step,
        partition: u32,
        body: &[u8],
    ) -> Result<bool, relspill::Error> {
        let (ranges, done) = match step {
            Step::Scratch(len) => {
                self.scratch.resize(len, 0);
                return Ok(false);
            }
            Step::List(len) => {
                self.lists.0[partition as usize].0.resize(len, Entry::default());
                return Ok(false);
            }
            Step::Again(ranges) => (ranges, false),
            Step::Done { ranges, .. } => (ranges, true),
        };
        for range in ranges.as_slice() {
            let list: Vec<u8>;
            let from: &[u8] = match range.region {
                Region::Buffer => &self.buffer,
                Region::Scratch => &self.scratch,
                Region::Body => body,
                Region::List(partition) => {
                    // A list's entries as they lie in memory.
                    list = self.lists.0[partition as usize]
                        .0
                        .iter()
                        .flat_map(|entry| [entry.offset, entry.stored])
                        .flat_map(u64::to_ne_bytes)
                        .collect();
                    &list
                }
            };
            let (at, len) = (range.at as usize, range.len);
            if self.file.len() < at + len {
                self.file.resize(at + len, 0);
            }
            self.file[at..at + len].copy_from_slice(&from[range.start..][..len]);
        }
        if !ranges.as_slice().is_empty() {
            writer.written()?;
        }
        Ok(done)
    }
}

/// Bytes of the file read into a page of entries, as they lie in
/// memory: words of 8 bytes, two to an entry.
fn fill(page: &mut [Entry], bytes: &[u8]) {
    for (index, word) in bytes.chunks_exact(8).enumerate() {
        let mut value = [0; 8];
        value.copy_from_slice(word);
        let value = u64::from_ne_bytes(value);
        let entry = &mut page[index / 2];
        if index % 2 == 0 {
            entry.offset = value;
        } else {
            entry.stored = value;
        }
    }
}

fn main() -> Result<(), relspill::Error> {
    // Two partitions of the first level, chunks of 64 KiB at most, and a
    // write buffer of 4 KiB. The fingerprint names what the chunks hold:
    // a reader of another refuses the file.
    let config = Config {
        partitions: 2,
        level: 0,
        fingerprint: 0x5eed,
        bounds: Bounds::all(64 << 10),
        buffer_len: 4096,
        shared: false,
    };
    let mut writer = Writer::new(config)?;
    let mut host = Host {
        buffer: vec![0; config.buffer_len],
        scratch: Vec::new(),
        lists: Kept(vec![(Vec::new(), 0); 2]),
        file: Vec::new(),
    };

    // Chunks of values, bytes the crate keeps as they are: a multiple of
    // 8 bytes, numbered 0, 1 and 2.
    let chunks = [(1, [1_u8; 16]), (0, [2; 16]), (1, [3; 16])];
    for (number, (partition, body)) in (0..).zip(&chunks) {
        let block = Block {
            kind: BlockKind::Values,
            number,
            partition: *partition,
            body,
        };
        loop {
            let step = writer.write(&mut host.buffer, &mut host.scratch, &mut host.lists, block)?;
            if host.serve(&mut writer, step, *partition, body)? {
                break;
            }
        }
    }
    // The finish writes what the buffer holds, the lists and the trailer.
    loop {
        let step = writer.finish(&mut host.buffer, &mut host.lists)?;
        if host.serve(&mut writer, step, 0, &[])? {
            break;
        }
    }

    // Partition 1 back, from the lists at the end of the file. The
    // reader asks for pages of entries, then for each block whole.
    let read = ReadConfig {
        partitions: 2,
        level: 0,
        partition: 1,
        fingerprint: 0x5eed,
        bounds: writer.read_bounds(),
    };
    let mut reader = Reader::file(read, host.file.len() as u64)?;
    let file = &host.file;
    let bytes = |at: u64, len: usize| &file[at as usize..][..len];
    let mut page = [Entry::default(); 16];
    let mut block = loop {
        match reader.open(&page)? {
            ReadStep::Page { at, len } => fill(&mut page, bytes(at, len)),
            ReadStep::Ready { largest } => break vec![0; largest as usize],
            step => unreachable!("{step:?} while opening a partition of blocks"),
        }
    };
    let mut back = Vec::new();
    loop {
        match reader.next(&page, &block)? {
            ReadStep::Page { at, len } => fill(&mut page, bytes(at, len)),
            ReadStep::Block { at, len } => block[..len].copy_from_slice(bytes(at, len)),
            ReadStep::Header(header) => {
                let mut body = vec![0; header.len as usize];
                reader.body(&block, &mut body)?;
                back.push((header.number, body));
            }
            ReadStep::End => break,
            step => unreachable!("{step:?} while reading"),
        }
    }
    assert_eq!(back, [(0, vec![1; 16]), (2, vec![3; 16])]);
    Ok(())
}
```

## The parts

- **The format.** A block is a header and a chunk: a chunk of a
  relhash table's records, packed lane by lane; a chunk of the values
  that records and rows refer to; or a chunk of rows by column, packed
  by frame of reference. A set's file holds the blocks, the lists of
  its partitions and a trailer. The modules `set` and `columns` and the
  items `BlockHeader` and `pack` draw every layout.
- **The decisions.** `shared_spill` keeps the words in which a table
  that spills counts its partitions and marks those on disk, alone or
  shared by several processes; `plan` plans each level; `phases` steps
  a build by several processes and its rounds over the partitions on
  disk.
- **The machines.** `join`, `group` and `sort` are the spills of a
  hash join, of a grouping and of a sort's runs; `shared` is a build by
  several processes that may spill. Each machine is plain words the
  caller keeps where it likes, such as in memory it shares with C.

## What the caller promises

The crate has no `unsafe` code. A caller that keeps the words of a
machine, of a set or of a spill in memory it shares with other
processes makes the slices of them itself, and promises what such a
slice needs: words that every process reaches only through atomics,
for as long as the slice lives, and words of its own that no other
thread touches while a call runs. Whatever bytes the words, the lists
and the file hold, a call ends, returns a result or an error, and
reads and writes no memory but the regions it is given.

The format has no checksum: a temporary file is read back by the query
that wrote it, on the same machine, once its writers have finished.
A block read while a writer still writes it, or bytes damaged so that
every check still passes, may be taken for other data.

## Building

relspill builds for targets whose pointers are 64 bits wide, and on any
other target fails to build with an error that says so.

## How it is built

The [design] says how the spill is built and why, for those who change
it; a caller does not need it.

[Tessera]: https://github.com/darthunix/tessera
[relhash]: https://github.com/darthunix/tessera/tree/main/crates/relhash
[design]: https://github.com/darthunix/tessera/blob/main/openspec/specs/relspill/design.md
