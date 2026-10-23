## Context

This design is about the move. Its folder goes when the change closes,
and what a reader of the crate needs is then written into the design
of `relhash` (task 7.1); nothing of this context goes there.

The move cuts these ties of the table to the rest of Tessera:

- `exclusive.rs`, the folds of aggregate states into a payload:
  `crate::decimal` and `crate::ops::ArithmeticError`.
- `order.rs`, the items of a sort: `crate::sort`.
- `keys.rs`, keys from columns: `tessera_core::ColumnReader` and
  `WordBlock`.
- `lanes.rs`, the prefetch hint: `crate::simd::prefetch`.
- Every file: `tessera_core::{RowMask, RowMaskView, ones}` and
  `anyhow`.

`header.rs`, `region.rs`, `record.rs`, `phases.rs` and
`shared_spill.rs` need nothing outside the table. The workspace builds
without LTO, so a small function of another crate is inlined only when
it is generic or marked `#[inline]`: the calls the crate gives
Tessera's loops are generic for that reason.

## Goals / Non-Goals

**Goals:**

- The table as a crate that a database engine outside Tessera can take
  as it is.
- Not one instruction more in a hot loop of Tessera.
- The promise "any bytes, a bounded call" shown by proofs and tests
  that cover every place the table trusts its own memory.

**Non-Goals:**

- See "What we do not do".

## Decisions of the maintainer

On 2026-10-10, in a discussion of the table and in the review of this
proposal:

- The crate lives in this workspace and is named `relhash`. "Rel"
  stands for the relative references that replace addresses, and for
  the relational engines the table is built for. Names that begin with
  `tessera-` were refused: on crates.io about 120 crates carry that
  word, a family of them belongs to a user interface framework, and
  such a name would suggest that the crate needs Tessera.
- Its API is by batches only, and the batches are the crate's own types,
  free of Tessera.
- The Bloom filter is a module of the crate.
- The marks of RIGHT and FULL joins go into the crate: a building block
  of the hash table of any database engine.
- The phases of a shared build stay in Tessera.
- Kani is added to the checks.
- Fuzzing runs briefly on every pull request, and for one to two hours
  before a release of the crate.
- The crate's documentation recommends a build with LTO.
- The promise over any bytes holds for a table that no other process or
  thread writes while a call runs, over blocks aligned to 8 and
  initialized.
- The crate has no walk over records of its own: the items of a sort
  are made through the scan and the cursor that reads a record.
- 32-bit targets are of no interest: the crate builds for 64-bit ones
  only.
- The spec of the crate comes in changes of its own: two move the
  requirements of `hash-table` that describe the table itself into the
  capability `relhash`, nine each; this change adds the new promises.
- The crate's documentation does not repeat its spec: it links to it.
- The table that owns its memory stays in the crate, behind a feature
  `alloc` on by default, outside the promise of no memory. Nothing of
  Tessera's product uses it, but it is the crate's way in without
  unsafe code, and the table of its tests and benchmarks.
- The work goes as small pull requests into a branch `relhash`, each
  one the maintainer reads, and the branch goes into `main` whole at
  the end.

## Decisions

### What goes into the crate

**Was:** one module of `tessera-kernels`, with the folds of aggregate
states, the items of a sort, keys from Tessera's columns, the phases of
a shared build and the words of a spill among the parts of the table.

**Will be:** the line between the two is whether a part knows SQL or
Tessera's executor.

- Into the crate: `header.rs`, `region.rs`, `record.rs`, `batch.rs`,
  `local.rs`, `bloom.rs`, `marks.rs`, `lanes.rs`, `mod.rs`; of
  `keys.rs`, the trait of a source of keys; of `exclusive.rs`, find or
  insert, its form by partition, grouped link, scan, regrow and clearing
  a key; of `loom.rs`, the models of the table, the filter and the
  marks; `model.rs`, the memory of those models and the model table.
- Into the crate from `tessera-core`: `row_mask.rs`, `bits.rs` and
  `bitmap.rs` (see "The masks of rows").
- Stay in the kernels: the folds, sums and extremes of `exclusive.rs`
  and the kinds of state its merge knows; `order.rs`; of `keys.rs`, the
  reading of keys from Tessera's columns, where `KeyColumns` stands for
  a slice of columns as a source of keys, since an implementation of
  the crate's trait for a slice would be the kernels' for no type of
  their own, which Rust refuses; `phases.rs`;
  `shared_spill.rs`; of `loom.rs`, the models of the phases, the rounds
  and the words of a spill.

Refused: the folds in the crate, since they carry PostgreSQL's numeric
and its int8 error; the phases in the crate, since they are the steps
of a join over PostgreSQL's barrier, with steps of a spill.

### The public API

**Was:** about 145 public items, the table's under `relhash::table`
and the masks at the root. Most mirror the C API, which alone calls
them; Tessera's kernels take a few for the items of a sort and the
folds, and only tests use the owning table. The calls over chunks
before a table has an index are four free functions, each given the
configuration again. A reference is a `u32` that parameters and texts
call an offset. The format's magic is `TESSTABL`. The owning table
opens hooks for tests to read and damage its blocks.

**Will be:** in the maintainer's review of the API, before its
snapshot:

- The table is the crate: its items lie at the root, `relhash::Table`,
  `relhash::OwnedTable`, `relhash::bloom`, beside the masks. Tessera's
  kernels keep `tessera_kernels::table` as a name for the crate.
- The calls over chunks before a table has an index are one type made
  over a configuration and chunks, `ChunkWriter`, with `append`,
  `append_columns`, `append_partitioned_columns` and `split`: the
  configuration is checked once, and the name says there is no index
  yet.
- References are called references, in parameters and texts; they
  stay `u32`, as C passes them.
- The magic is `RELHASH\0`. The format stays version 2, which no table
  with `TESSTABL` had.
- `BITS_PER_RECORD` closes: `words_for` sizes a filter.
- The hooks for tests leave the public API when the table's tests move
  into the crate, before the snapshot of the API.
- Unchanged: the raw pointers of the unsafe way in, since other
  processes write shared memory while a call runs, which a slice would
  forbid; `MAX_CHUNKS`, a limit callers keep; `SharedFilter::from_mut`,
  the safe way to a filter in one process.

Refused: a type of its own for a reference, which every array that
crosses the C edge would convert; the four free functions.

### A lookup as one value

**Was:** a probe took five arguments: the hashes, the keys, a mask of
the rows to look up, the references it found and a second mask of the
rows found. The next match and the next in a group took the references
and two masks again, and each gather the references and a mask. A
caller made each mask of its words and a wrapper, and remade both for
every step of a walk down the matches.

**Will be:** in the maintainer's review of the API, after a study of
the batch hash tables of DuckDB, Velox, ClickHouse, Arrow, DataFusion
and Polars:

- `Matches` holds a lookup's rows, a mask of the caller's words, and the
  reference of each row's record, at its row. It checks their lengths
  once, when it is made. Velox's `HashLookup` and DuckDB's
  `ScanStructure` are such values, kept for a whole operator.
- The calls narrow its one mask in place, as Arrow's `find` narrows its
  `inout_match_bitvector` and as a batch's pending rows shrink: a probe
  keeps the rows that find a record, a next match those that find one
  more, and a Bloom filter's probe those it lets through, before the
  table's.
- Each returns how many rows are left, so that a walk is
  `while table.next_match(&mut matches)? > 0`.
- The gathers take the `Matches`, so that the references and the rows
  they read can never disagree. A caller whose references come from
  elsewhere, such as the items of a sort, makes a `Matches` of them.
- The hashes and the keys stay arguments of the probe: a gather needs
  neither.

The C API takes two masks, the rows and the results, or one mask as
both. Given two, an entry point copies the rows into the results, a
word for 64 rows, after the call's checks, so that a call refused
leaves its results as they were, and calls the same. The copy cost a
join with four records a key about 2 per cent: its node calls a lookup
four times a batch of 64 rows, and each call copied, checked and
counted for about 13 cycles more than a call that wrote its results
whole. Given one mask, or two masks of the same words, an entry point
narrows it in place and copies nothing; the join and the grouping call
so wherever they kept a copy of the rows only to give it to the call.

Refused: a list of (row, record) pairs with a limit and a point to go
on from, as Velox, Arrow, DataFusion and Polars give their joins; a
lookup moves each row one record a call, and its references are where
it goes on. Two masks in the value, which would keep the old loops but
move each call's result into the next call's rows.

### A batch to place as one value

**Was:** a call that places rows takes a `Batch`: the hashes and the
keys of its rows, the mask of the rows still to place, and the
reference of each placed row's record, at its row. The documentation
of `Batch` says why the mask exists and how a batch lives through
several calls, for a caller of the crate, whom it may surprise. Three
things in the form of the value made it harder to use than a lookup:

- The batch borrows the mask as a `&mut RowMask`, which itself borrows
  the caller's words: two lifetimes, and four or five lines to make a
  batch, the words, the mask, the references and the batch. The
  references are `refs` in `Batch::new` and `offsets` in the rest of
  the crate.
- `OwnedTable::insert` and `OwnedTable::find_or_insert` take the same
  batch, though they add chunks and grow the index themselves and
  always leave the mask empty. Their caller copies the rows to place
  into a mask only to see it cleared.
- `find_or_insert` and its form by partition, in the crate and in the
  C API, fill the mask of inserted rows whole at each call. A caller
  that calls again after a full chunk must add the masks of the calls
  up itself, or the groups made before the chunk ran out lose their
  initial states. The grouping node clears a mask of its own before
  each call and adds it into the batch's; the owning table does the
  same with a vector it allocates.

**Will be:**

- `Batch` is made as `Matches` is: it holds its mask by value, has one
  lifetime, and is made from the caller's words, `Batch::new(hashes,
  keys, pending, refs)`, or from a mask, `Batch::from_mask`, as the C
  entry points have one. Its references are `refs` in the whole crate.
- The calls of `Table`, `TableMut` and `ChunkWriter` keep the mask of
  pending rows: it is where the next call goes on, and what a call by
  partition leaves has gaps that a row number would not say.
- `OwnedTable::insert` and `OwnedTable::find_or_insert` take the rows
  to place as a mask they only read, with the hashes, the keys and the
  references, and keep the mask of pending rows to themselves.
- `find_or_insert` and its form by partition add the rows whose record
  they made to the mask of inserted rows, and leave its other bits as
  they were. The caller clears it once, before the first call of a
  batch. The grouping node and the owning table stop adding the masks
  up.

Refused: the number of the row to go on from, for the calls into one
chunk, which place a run of rows from the start; the calls by
partition need the mask, and two ways to go on for calls side by side
are more to learn than one mask. A call that asks the caller for a new
chunk and never stops: a larger index changes the table under the call
that holds it, and a partition that fills would have to spill from
inside the call, which is far more across the C boundary than a mask.
Dropping the count a call returns, which the mask tells as well: the
owning table checks by it that an empty chunk took a record.

### What an outside review found

A review of the whole crate by another model, after the tests had moved
into it, found no path by which damaged bytes lead a call over the
caller's blocks to undefined behavior, a panic or an endless loop. It
found these at the edges of the promise, the first three reproduced,
and the maintainer took all of them, with its advice on the interface,
the documentation and the tests.

- **A call that fails part way.** `append_rows`, `append_column_rows`
  and `resolve_rows` store a chunk's used mark once, after the loop
  over words. A source of keys that cannot give the second word fails
  the call with the first word's records written, their references
  given and their pending bits cleared, but the mark where it was: the
  next append writes over them, and in `find_or_insert` over records
  already counted and linked. The calls by partition clear the pending
  bits of a word only after it, so a partition's chunk refused in the
  middle of a word leaves rows placed but pending, which a retry places
  twice; `link_counting` adds the count of the whole range before it
  checks a record. Tessera never reads a table after an error, since
  every error ends the query, but a caller of the crate may.
  **Will be:** every exit of these calls stores the marks of the chunks
  it wrote, and the masks say what was done: the rows placed before the
  error have left the pending mask with their references, and those
  that made a record are in the mask of inserted rows; a link counts
  what it linked and moves `*from` past it.
- **The owning table.** `find_or_insert` and `regrow` do not move the
  place up to which `insert` has linked a chunk, so an `insert` after
  them links records a second time: their count grows past the records
  and a chain turns into a cycle. A chunk too small for a record makes
  `find_or_insert` add chunks up to the limit of 32768 before it fails,
  and with another rule than `insert`. **Will be:** the owning table
  keeps no such place: an insertion links what it appended, from the
  last chunk's used mark before it, and nothing else, whatever other
  calls appended, linked or found before; `OwnedTable::new` refuses
  such chunks. Refused: moving the kept place after every other call,
  which each new call would have to remember.
- **A borrowed record and a write.** `Table::append`, `append_columns`,
  `link` and `link_counting` write chunks and the index through a shared
  `&Table`, the handle a shared build needs; the rule of one writer a
  chunk lives only in `Table::attach`'s contract. `OwnedTable::table` is
  safe, so safe code can borrow a record, after a `reset` by an old
  reference, and write the same bytes with an append while the borrow
  lives, which Miri reports. **Will be:** these calls are safe on
  `TableMut`, which has the table to itself, and `unsafe` on `Table`,
  whose contract names the chunks the caller writes and that no record
  of them is borrowed; `scan`, which reads the marks that writers move,
  moves to `TableMut`, by `&self`, since a sort walks the records while
  a cursor reads them. Refused: a handle of its own for a participant
  of a shared build, which would add a type for what a contract on four
  calls says.
- **The keys of NULL rows.** A record's slot of a NULL key holds 0, and
  keys are compared slot by slot, but the table takes the slot from the
  source as it is: a source that leaves a value there splits the
  group of NULL keys. **Will be:** the source's duty stays, as
  `KeySource::word` says, and a build with debug assertions checks it.
  Refused: the table storing 0 in those slots, which the counters showed
  at one or two instructions a NULL row, up to 2.8 per cent of a batch of
  NULLs; every source of Tessera gives 0 already.
- **A cycle under a damaged count.** A walk down a chain stops after as
  many steps as the count of records, which `refresh` bounds by 2^17 a
  chunk; over hundreds of chunks a cycle with a damaged count takes
  hours a batch. **Will be:** a walk stops at a cycle by Brent's method,
  a compare a step and a saved place at each doubling of its steps. The
  method runs only past 64 steps, in a function out of line, which a
  word's probe hands its rows still walking to: below that the loops
  compare the steps once a step against the smaller of the record count
  and 64, as they compared them against the count alone. The counters
  show what this costs a healthy table, and the cost is accepted. A
  found row costs 0.6 to 0.9 instructions fewer, and a probe that misses
  0.1 more. A call of a find or insert that finds few rows costs 36 to
  56 instructions more, out of 430 to 730, and about 13 more a word of
  one row: the call keeps more of the access on the stack. A probe pays
  5 to 8 more a call and 1 to 3 a word of one row. A full word of 64
  found rows costs what it did. Refused: the access keeping the bound,
  set as it reads the count. That saved 19 instructions a call, but took
  a register from the walk, which then read a pointer from the stack at
  each step: 1.2 instructions a found row more.
- **The interface.** `Marks::set` and `Marks::word` become safe: the
  crate keeps their word within the chunk's marks, and says so on the
  trait. `check_rows`, a helper, leaves the crate's API; `tessera-core`,
  whose kernels check the rows of their own arrays, keeps one of its
  own with the same text. `init_chunk` and `chunk_used` become
  `Chunks::init` and `Chunks::used`, beside the other calls over chunks.
  `CombineStop` is `MergeStop`, for `merge`; the C API keeps its enum.
  `Batch` gets `refs`, as `Matches` has, and `Batch::new` an example that
  binds its source of keys. `Table::mark` and `Table::scan_unmarked`
  take the record size and the chunks from the table, which a caller
  cannot get wrong; `mark` and `scan_unmarked` stay for chunks without
  an index, such as a partition read back from disk.
- **The documentation.** The calls a rule of one writer binds say so
  where they are; `record`, the gathers and the next match say what a
  reference past a chunk's used mark reads while a writer writes it;
  `KeySource::word` says what a source that cannot give a word leaves
  behind.
- **The tests.** Most refusals were checked by `is_err()`, which a
  wrong rule or kind passes: they compare the kind and the rule. New:
  sources that fail at a chosen word, random sequences of the owning
  table's calls against a model, the longest chunk filled to its last
  record, chunk 32767, the largest record end to end, a loom model of
  `link_counting` beside a third participant's link, Miri over the doc
  tests, and a build without `alloc` free of warnings.

### What a second review found

A second review by the same model, after the fixes of the first,
found them sound and the walk, the checks of the format and the
publishing of records as before. Two faults were left at the edges of
the safe interface, both reproduced, and gaps in the contracts and the
documentation that a caller embedding the crate would fall into. The
maintainer took all of them.

- **A chunk writer and a borrowed record.** The writes left the shared
  `Table`, but `ChunkWriter`, which writes the chunks of a table that
  has no index yet, stayed safe to make and to call. `Chunks` is
  `Copy`, so a `Table` and a `ChunkWriter` over the same chunks, each
  made within its contract, let safe code write a record that
  `Table::record` has lent, which Miri reports. **Will be:**
  `ChunkWriter::new` is `unsafe`: while the writer lives, it is the one
  writer of the chunks it writes, and nothing borrows a record or a
  payload of them while one of its calls runs. `Chunks::new` says that
  a chunk has one writer across every table and chunk writer over it.
  Refused: its four writes `unsafe` and the making safe, which would
  repeat at every call what the maker knows once.
- **An insertion of the owning table that fails part way.**
  `OwnedTable::insert` links what it appended only after the append
  returns, so a source that fails at a word leaves the records of the
  words before it in the chunk and never linked: a probe misses them, a
  walk sees them, and the same rows given again are appended twice.
  **Will be:** the insertion links what it appended before it returns
  the error. Its documentation and that of `find_or_insert` say what an
  error leaves: the rows of the words before the failed one are placed
  and found, and the same rows given again to an insertion are placed
  again, as a join's table allows.
- **The calls that read every chunk's used mark.** `bloom`,
  `try_build_bloom` and `Table::scan_unmarked` read the used mark of
  every chunk, other participants' too, by plain loads. Their
  documentation asks for no chunk taking records, but they are safe
  calls, and no contract of an `unsafe` call forbids running them
  beside another participant's append: a data race, with every
  contract met. **Will be:** the contracts of `Table::attach` and
  `Chunks::new` name these calls and say they run only once every
  writer's appends are done and ordered before them, as by a barrier,
  which is what Tessera's shared build does. Refused: a used mark read
  and written atomically. It would make the read defined but not the
  answer: a filter or a walk beside an append misses the records after
  the mark it read, and the barrier that makes the answer right orders
  plain loads already.
- **A call that fails after it changed the table.** `regrow` and
  `clear_key` meet a chunk that cannot be one only when they reach it,
  after they changed the chunks before it, though a wrong argument is
  promised to fail before the call changes anything. And a find, a
  merge or a regrow that meets damaged bytes in the middle returns at
  once: the records before it may lie past the used mark, and a caller
  that goes on writes over them. **Will be:** `regrow`, `clear_key`,
  `bloom` and `try_build_bloom` check every chunk before the first
  change. A table that a call found damaged is to be dropped or reset:
  the crate promises that later calls over it stay bounded and sound,
  as over any bytes, but not what they answer. Refused: storing the
  marks on those exits too. The chains are damaged whatever the marks
  say, and a stop on that path would cost the hot loops, as the stop
  of a call that fails part way did.
- **Smaller faults.** A walk given a cursor past its chunk's used mark
  skips the rest of the chunk; it fails with the rule a walk of
  unmarked records uses. The most records of a find or insert and of a
  merge, half of 2^31 buckets, is written on those calls and on
  `regrow`, and the owning table at that limit fails with a wrong call
  instead of growing to the same index again and again. A shared
  filter whose build fails stays building: it never reads ready,
  probes refuse it, and nobody waits; `try_build_bloom` says so.
  Refused: setting the state back to none, which lets the next
  participant fail the same way. The chunks given to `regrow`, `bloom`,
  `clear_key` and a walk are the table's whole set of records: `regrow`
  links records not linked yet, a link of them afterwards makes a cycle
  of one record, and a merge's source leaves the list before a regrow;
  their documentation says so.
- **The interface before it is frozen.** `OwnedTable` owns its
  blocks, and every handle to it borrows it, so it may pass to another
  thread: it becomes `Send`. The types the crate gives back, `Stats`,
  `Split`, `Record` and `MergeStop`, become `#[non_exhaustive]`, so
  that a count or a reason can be added later; the types a caller
  builds, `TableConfig`, `Partitions` and `Appended`, stay open, and
  `Kind` keeps its two kinds. A reference the caller passed that lands
  inside a record or on unused bytes reads as a damaged table, since
  the crate cannot tell it from damage: `Kind::Damaged`, `record`, the
  gathers and the next match say so. `KeySource::word` keeps its
  `Option`: a source that must say why it failed keeps its own error,
  which the caller reads after the error of a key it could not read.
  The documentation of `Cursor::from_raw`, of `Marks::set` (by `&self`,
  so an implementation needs interior mutability) and of `Matches`
  after a key not read in the middle of a probe fills the gaps found.
- **The tests.** Every test of the walk past 64 steps has a cycle, so a
  mutant of Brent's step that took every long chain for one passes
  them: a healthy chain of 300 records of one hash is walked to its
  end, by row, by word, down its matches and by a find or insert. The
  owning table's model gets sources that fail at a chosen word. The
  crate's tests that need no owning table run without `alloc`.

### What a third review found

A third review, by another vendor's model, confirmed the second
review's findings and the decisions above, and found what both missed:
two faults of soundness, both reproduced under Miri, a walk over
matches that a cycle keeps going, and smaller faults. The maintainer
took all of them.

- **Pointers to the payloads of one group.** `tess_table_payloads`
  gives C a pointer to the payload of each selected row's record, each
  made from the `&mut [u8]` that `each_payload` lends a row. Two rows of
  one group get two pointers to the same bytes, and under Rust's rules
  of aliasing the second loan ends what the first pointer may do: a
  write through it after the call is undefined behavior, which both of
  Miri's models report. Grouping does this on every batch whose rows
  repeat a key. **Will be:** the crate gives the pointers itself: a call
  that checks each reference as `each_payload` does and writes a raw
  pointer to its payload, made from the chunk's base without a loan of
  the bytes, so that two rows of one record get the same pointer, each
  usable until the chunk is reset. `each_payload` stays for a caller
  that is done with one payload before the next. Refused: a promise
  that the references of a batch are distinct, which grouping cannot
  keep.
- **The memory the contracts must name.** The contracts of
  `Chunks::new`, `Table::attach` and `TableMut::create` ask for blocks
  that are valid, aligned and reached only through tables, but not that
  they are apart: two chunk numbers over the same bytes let a merge
  borrow a record as its source and as its destination at once. And a
  `TableMut` has its index to itself, but not its chunks: a `Table`
  over another index and the same chunks may borrow a record that the
  `TableMut` changes. **Will be:** the contracts say that the index,
  the chunks and the arrays that name them do not overlap, and that a
  table that has itself to itself has its chunks too, whatever other
  index names them; the C contracts say the same. Refused: checking for
  overlaps at run time, which an `unsafe` constructor leaves to its
  caller.
- **A walk over matches that does not end.** The stop at a cycle bounds
  one search for the next match. It does not bound a walk over all the
  matches of a key, which a join makes one call at a time: a record
  whose next names itself, or a cycle of records of equal keys, gives
  the same matches call after call, each call bounded. **Will be:**
  `Matches` counts the steps to a next match or a next in a group that
  its rows have taken; a lookup cannot have more of them than the table
  has records, so a step past that count fails as a damaged chain. One
  compare a call, not a row. In C the count is an argument of
  `tess_table_next_match` and `tess_table_next_in_group`, which the
  caller sets to 0 with its probe. Refused: leaving the bound to the
  caller, which every join would have to write again.
- **Smaller faults.** A count of rows by partition that the caller
  gives at `u64::MAX` overflows after the row is written: a panic in a
  debug build, a count of 0 in a release one; it fails with a wrong
  call before the row, and the rows before it stay placed. The C gather
  of more than 16 words checks each group of 16 as it reaches it, so a
  word past the payload fails the call after the first groups were
  written; it checks the whole range first. A walk into an empty output
  returns 0, which means the end; it fails with a wrong call.
- **The texts.** `Matches::rows_mut` lends the mask itself, so a
  caller may put in its place one of another size: the calls check the
  shape every time, and the documentation stops saying it is checked
  once. A merge's rule that fails after it changed a payload leaves the
  payload as it left it; the merge says so, and a table after an error
  of the caller's rule or a caught panic is the caller's to judge, not
  a damaged one. Six public fields get their documentation, a mask's
  error no longer says it may allocate, and the documentation without
  `alloc` builds, its example over the caller's blocks.

### The kinds of keys

**Was:** a table was created with a kind for each key, `int4` or
`int8`, which its header kept in a byte a key and its fingerprint took
in. The table never read it: every key is a word of 8 bytes compared
bit for bit, and the source of keys widens an `int4` into its word.
Only the items of a sort compared their keys' kinds with the table's.

**Will be:** in the maintainer's review of the move, the table keeps no
kinds. A configuration is a count of keys and a payload size; the bytes
of the header that held the kinds are reserved and 0, and the format is
version 2. The fingerprint guards what it guarded, the layout of a
record's bytes, which the kinds never changed. The C calls that size,
make and split a table, and the configuration of Tessera's rows, take
no kinds; the nodes keep theirs for reading key columns and for the
items of a sort, whose kind of key is the kernels' own. The crate then
holds no name of a PostgreSQL type, as its contract asks.

Refused: a byte a key that the caller sets and the crate keeps without
reading it, which would keep a field of the format for one check of a
sort.

### The masks of rows

**Was:** `RowMask`, `RowMaskView` and `ones` live in `tessera-core`,
with the checks of `bitmap.rs` that its columns use too; every kernel
uses them. The columns now count their words and check a row
themselves, so the crate keeps those checks to itself.

**Will be:** they move into the crate, which owns its batch types.
`tessera-core` exports them again under the same names, so no other code
changes its imports. A view of a mask is its row count and its words. It
also read a window of bytes from a bit offset, the form of Arrow's
bitmaps, but nothing in Tessera built one, so that form is gone; it
comes back with a source that hands masks over in it. Its absence costs
time all the same. The compiler had laid out each kernel's loop over the
words of a mask twice, once a form, and the copy for words kept its
values in registers across the call of the vector kernel of a word; with
one form left it lays the loop out otherwise and saves and restores them
around that call. By the processor's counters, aggregates over whole
words of 1024 rows take up to 12 % more instructions and a count over a
Datum column 19 % more cycles, arithmetic up to 2.5 % more instructions,
while batches of 65 rows and counts over dense columns are faster. The
maintainer kept the masks without the form and took this cost; loops
that keep their values across the call, by their own form, are later
work. The walk over the ones of a word is a function that returns a
plain iterator, so the crate shows the walk and not the type of its
state. Reading a word past a mask's words still panics, as it does now:
a check there costs 4 to 9 instructions a word in the kernels' loops. It
is a call over a mask, not over a table's blocks, so the promise over
any bytes does not reach it.

Refused: a mask of the crate's own beside the old one, two copies of
one type; a third crate for the masks alone, one more crate to publish
and keep for about 450 lines. The cost: `tessera-core` now depends on
`relhash`, which a reader may find upside down.

### Errors of two kinds

**Was:** `anyhow::Error` with a text. The files that move build one in
about a hundred places. `tessera-capi` passes any of them on as
`TESS_ERROR_INVALID_ARGUMENT` with its text.

**Will be:** an `Error` of two kinds, a wrong call and a damaged table,
each a closed list of the rules a call checks; the spec lists them. The
kind follows where the broken value came from: an argument makes a wrong
call, a byte read from the table's blocks a damaged table. The error
carries a number or two that explain the case, no text and no
allocation; its `Display` writes the message the C side passes on, so
what PostgreSQL shows stays as it is. One check may give either kind: a
reference is checked by one function whether the caller passed it or the
call read it from a bucket or a record, and the call site, which knows
where it came from, chooses the kind. The one error today that names no
rule, a header the fast check and the full one judge apart, becomes a
debug assertion: the two checks are one rule written twice, and in a
build without assertions the full check decides. A merge takes the
caller's rule with an error of the caller's own type and returns that
type; the crate's errors convert into it, as they do into
`anyhow::Error`. A source of keys that cannot give a row's key answers
`None`, and the call names the rule with the word of rows and the key.
Tests compare kinds and rules, not words.

The error takes at most 8 bytes, and a compile-time assertion keeps it
so. Then a `Result<usize, Error>` stays 16 bytes, as with
`anyhow::Error`, a single pointer, and the hot calls keep returning it
in two registers. A larger error would be returned through memory. The
error is one integer, not a struct: a result holds an integer and a
count as two registers, and a struct, even of 8 bytes, through memory.

```text
  bits 56..=63   the rule's number, counted from 1, so never 0
  bit  55        the kind
  bits 23..=54   the first number
  bits 0..=22    the second number
```

The first number holds a reference or a length up to 4 GiB, the second
a byte of a chunk or a count. A number wider than its field keeps the
field's largest value, and the message prints it with "or more". Of a
message that had three numbers, the error keeps the two the caller
does not know; the check of a call's row counts names the two counts
that differ, not the call. The rules are listed once, in a macro that
writes both the enum and the table that turns a number back into its
rule, so that no `unsafe` is needed.

Refused: keeping `anyhow` and looking for words, which allocates and
does not tell the checks apart. The rule in a byte of its own beside
seven bytes of the rest: the lookups returned their counts through
memory, and the walk of a key's records spent a load a row on the
address of the result. An error type of the source of keys, which would
make every call with keys generic in its error.

### The calls for a caller's payloads

**Was:** the folds and the items of a sort use the table's inner parts:
the access to a record, the place of a payload, the memory under it.

**Will be:** two calls, each generic and inlined, so that the loops of
Tessera compile as they do now:

- **Cursors by reference.** A cursor is made once a call and keeps
  what the table's access keeps. One reads the record a reference
  names. The one writer's other cursor gives the payload a reference
  names, to change; it takes itself by `&mut`, so two payloads are
  never open at once, even for two rows of one record. For the sums
  that keep the payloads of a few groups at hand, it checks a reference
  now and opens its payload later. The items of a sort take their
  references from the scan and their keys through the reading cursor.
- **A merge by a rule.** The loop that merges groups read back from
  disk today, with the rule given as a closure.

Refused: making the inner access public and unsafe, which would put all
of it into what must be proven; keeping the folds in the crate, see
above; a walk over records of its own, which the scan and the cursor
already give. If a loop of Tessera compiles longer through these calls,
other forms are tried as "Stop and ask" of `CONTRIBUTING.md` says, each
a hypothesis written down first. When none keeps the loop as short,
the cost in speed against the crate's boundary is a trade-off no rule
settles, and the maintainer decides it.

The items of a sort pay such a cost. Before, they walked each chunk by
place and checked only a record's length; through the scan and the
reading cursor, each record is checked by the scan and again by the
cursor, which takes its reference apart and checks the chunk, about
ten instructions more a record. A full sort is about 5 % slower; a
top-N sort, which read records by reference before, is not. The cost
is taken. The refused way: the scan hands out each record with its
reference, checked once, which would make the scan a walk over records
for its callers.

### The owning table

**Was:** `LocalTable` owns its index and chunks, allocates them, adds
chunks as they fill and makes a larger index when the records outgrow
it. The C nodes never use it; the tests of the table and of the C entry
points and the benchmarks of counters do.

**Will be:** it stays in the crate as its table that owns its memory,
behind a feature `alloc` on by default, named `OwnedTable`: owning its
memory is what sets it apart, while "local" read as a process's own
memory, which a table over a caller's blocks may be as well. It is the
only way to use the crate without unsafe code, so the README's example
uses it. It is
outside the promise of no memory: allocating is its job. A build
without the feature leaves it out, and has nothing that allocates.

Refused: moving it into the tests, which would leave a caller from
outside only the unsafe API; dropping it, which would make every test
and benchmark manage raw memory.

### The loom models

**Was:** `loom.rs` models the table, the filter, the marks, the phases
of a shared build, the words of a spill and the rounds. The models of
the phases and the rounds run the table's own code over the model of
its memory.

**Will be:** the crate keeps the models of the table, the filter and
the marks. The kernels keep the others. For them the crate gives, only
when built with `--cfg loom`, a model table: append, size, link and
probe over loom's atomics. A normal build has no such API. The phases
and the words of a spill keep the orderings of their atomics in modules
of their own, which their models share; the region's orderings stay
the crate's.

Refused: modelling the phases over a stand-in table. It would lose the
negative test where linking before the index exists breaks the table.

### The prefetch hint

**Was:** the probe of a batch asks the processor to fetch the next
memory through `crate::simd::prefetch` on AArch64, and does nothing on
other processors, x86-64 among them.

**Will be:** the crate has its own copy of the same hint, the same
instruction on AArch64, and nothing elsewhere. That x86-64 lacks it is
already planned by the roadmap's `simd-primitives-avx2`, among its
commits; once the table is in the crate, that commit is written there.

### 64-bit targets only

The crate's arithmetic of offsets assumes a 64-bit `usize`: the bytes of
the buckets, `4 * nbuckets` with up to 2^31 buckets, would wrap on a
32-bit target and pass the check of the index. Tessera runs on 64-bit
targets only, so the crate refuses to build on others with a
`compile_error!` that says why, instead of checking every product.

### What proves the promise over any bytes

The spec's "Any bytes, a bounded call" is the promise. It holds for a
table no other process or thread writes while the call runs. A shared
table that is being built breaks that condition on purpose: a call
checks a reference against the length of its chunk, not its used mark,
so as never to race with the chunk's writer, and a damaged reference
can point at bytes being written. For a shared table the spec promises
only what "Calls at the same time" says, over a table that is not
damaged; loom checks it.

Seven means show the promise, each covering what the others cannot:

1. **The trust points.** A trust point is a place where a call reads
   the index or a chunk. All such reads go through `Region`, so the
   list is finite. Each call site carries a mark, `// trust:` and a
   name, and a script fails when a read has no mark or a mark is not in
   the list. The list goes into the design of `relhash`: for each
   point, whether its value decides an address, the length of a loop,
   or only data; its check; its error; its proof; its row of the
   matrix.
2. **Kani.** A proof for all values, not for samples: a header that
   passes its checks puts every bucket inside the index; a reference
   that passes puts its record inside its chunk; a walk ends within its
   bound; a used mark that passes ends a record; a partition, a word of
   the filter and a word of the marks lie inside their arrays. The same
   runs show that these functions cannot panic. Whole calls are too
   long for Kani to unroll; the next two means cover them.
3. **The matrix of damage.** proptest builds valid tables: duplicate
   and NULL keys, several chunks, after a regrow and a split, with a
   filter and marks. One damage from a closed list goes into each,
   every field at and around its limits. Every call runs over it, the
   probe both row by row and by whole words, and then more calls after
   the error. A test checks the kind and the rule of the error, and
   guard words around every block that must not change. A few cases
   also run under Miri.
4. **Fuzzing.** cargo-fuzz over the same builder, with the input chosen
   by the fuzzer, under ASan. A pull request runs each target for a
   minute; a run of one to two hours goes before a release of the
   crate, on request. A finding becomes a case of the matrix.
5. **Mutants.** Every mutant of the crate, not only those of changed
   lines, and none survives but the equivalent ones, each listed with
   its reason. Since the tests compare rules, a removed check is seen.
6. **Miri** over all the tests of the crate.
7. **Tests of the rest of the spec:** every call under an allocator
   that counts; one sequence of calls run twice over two copies; a
   handle sent to another thread that does not compile.

### The spec of the crate, in three changes

**Was:** the spec of `hash-table` holds the table and what Tessera keeps
beside it, in the terms of the C calls.

**Will be:** once the code is in the crate, two changes make the
capability `relhash` and move into it the requirements that describe
the table itself, written again in the terms of the crate, with the
same tests, within the limit of ten a change:

- `relhash-spec-table`, nine: the index of a table, chunks and
  references, a record; appending rows, linking records, looking rows
  up, the records of a key together, calls of one writer, calls at the
  same time;
- `relhash-spec-partitions`, ten: partitions of a table, appending rows
  by partition, splitting a chunk by partition, groups by partition; a
  Bloom filter of the keys, its size, a shared filter, a filter filled
  together; the marks of RIGHT and FULL joins; and "A damaged table",
  what of "Errors of a call" the crate keeps.

`hash-table` keeps what Tessera builds around the table: the SQL types
of a key; the status and SQLSTATE of an error; the kinds of state of a
merge of groups; the counters, phases and rounds of a shared build; the
words of a spill, its split, its eviction, its records and files; a
partition read back that splits; the partitions and chunks of a level.
It gets one new requirement: each call of `tessera/table.h` does what
the crate's call does, pointing to the requirement of `relhash`, so
that no rule is written twice. The designs split the same way.

Neither change changes a test or adds a promise of its own. They move
the eight requirements that this change rewrites, as this change has
them, not as `main` has them: the first the index, a record, appending
rows and the calls of one writer; the second appending rows by
partition, splitting a chunk by partition, groups by partition and the
errors. In the crate's terms the old text would name a magic, a
version and kinds of keys the crate no longer has. Their entries leave
this change's delta, so these promises reach the main specs before it
closes; the maintainer chose this on 2026-10-23. Both changes close
before this one. Then this change's own requirements are set on top of
the moved ones: "Errors of two kinds" takes the place of "A damaged
table", by an entry that removes it, so that the rules of a damaged
table are written once.

### The branch and its pull requests

The work is long, and the maintainer reads every step. So it goes into
a branch `relhash` from `main`, a pull request for each task of this
change and of the two that move the spec, one to three commits each.
CI runs on every pull request, whatever its base. The moves of files
are split by groups of files, each a `git mv` with the edits of paths,
read through `git diff -M` and `cargo ab moved`.

- A move that takes a file named by the spec or the design of
  `hash-table` edits those paths in the same commit, so that
  `check-specs.sh` passes at every step of the branch.
- When `main` moves, the branch is rebased onto it, so that `main`
  never gets a merge commit. Work on the table in `main`, such as the
  prefetch hint of `simd-primitives-avx2`, waits for the branch, or the
  branch takes it right after.
- Once the crate is whole and an outside review's fixes are in, the
  branch comes into `main` fast-forward, every small commit as it was
  read, under a new hash. The tasks after it are pull requests into
  `main`, and the branch is no longer kept.

### The documentation of the crate

**Was:** the doc comments of the table restate its spec and its design:
the module of the header lists its fields by offset, and the module of
the table retells the design in about 50 lines.

**Will be:** each text has one reader and one job.

- The spec says what is promised: rules, layouts, limits, the rules of
  errors, the caller's promises, each with its test.
- The design says how the crate is built and why.
- The documentation is a map of the API for a caller: for each item,
  what it is for in a sentence or two, its parameters, who owns what
  and for how long, its `# Safety` duties in the terms of the code, the
  kind of error it returns, and examples. For a rule, a layout or a
  limit it links to the requirement of the spec, by an absolute link to
  the repository, so that the link works on docs.rs too.
- The crate's page is its README, taken into the documentation with
  `include_str!`, so that crates.io and docs.rs show one text: what the
  crate is for, an example without Tessera, the caller's promises in a
  line with a link, the advice to build with LTO and to use a hash with
  a secret seed for keys from outside, and links to the spec and the
  design.
- `#![deny(missing_docs)]` in the crate; CI builds the documentation
  with every warning an error, and runs its examples.

The links need the repository to be public when the crate is published.

### The contract of the crate

The crate is a new part, so it gets an architecture contract, as
`CONTRIBUTING.md` asks. Until this change closes the contract lives in
its folder, `contract.md` beside this design, and the checks read it
there; at the close it moves to `openspec/specs/relhash/`. Its rules
keep the crate free of Tessera and of SQL, its memory behind one layer,
its unsafe code where the proofs reach, every reference checked by the
crate alone, the masks of rows its own, its calls free of allocation,
its API small and its cost to Tessera nil; most are `pending` until the
task that writes their check. Its forecast names three likely changes:
a prefetch hint for x86-64, keys of more kinds, removing records. A
probe of each runs once the crate exists, on branches that are never
merged, and its count of what it touched goes into this design.

The grouping node of C breaks the rule of references today: it finds
the payload of a generic aggregate's state by an offset of its own,
after the crate has checked one record. It takes the payloads from
`tess_table_payloads` instead, one call for the rows of a batch that
need them. The check of each reference makes the loop of such a
grouping about 25 instructions a row longer, and a grouping by a
generic aggregate, such as `max` of text, about 5 % slower. That cost
is taken, as the one loop that grows against the rule of no cost to
Tessera. The refused way: the call that finds or inserts a row's record
hands out its payload too, which the crate has just checked. It saves
the second check, but changes the C API for this one loop.

The spills of the grouping and of the join read the used mark of a
chunk, its first 8 bytes, by themselves. They read it through a new
call of the C API instead, `tess_table_chunk_used`, which checks the
mark against the chunk's length and the size of its records, as
`tess_table_chunk_init` makes an empty chunk.

### Speed

Every commit is one of two sorts.

- **A move.** The machine code of every benchmark program and of the
  kernels' module is the same as the base's: `tessera-bench --base REF
  --disasm` and `--disasm --module`, with `cargo ab moved` for a file
  that is cut. Same code, same speed; no timing is needed.
- **A change**: the errors, the masks, the cursors, the merge. First the
  machine code, where no loop may grow without a reason written down;
  then the counters, run by the maintainer, and `cargo ab measure` on
  the families of joins, groupings and sorts.

Tessera builds without LTO, and all of this is measured without it.

### A fifth surface of a spec

**Was:** a spec names four surfaces: the C API, what SQL sees, formats,
and the entry points of the kernels.

**Will be:** a fifth, the public API of a crate of the workspace meant
for use outside Tessera, one with `publish = true` in its `Cargo.toml`,
a fact a reader can check; a change of such an API needs an OpenSpec
change, as one of the C API does. Then the spec of `relhash` can
promise what the crate's callers rely on. The same edit of
`CONTRIBUTING.md` says, beside the rule that a rule is written once in
the spec, that a crate's documentation links to its spec and does not
repeat it. All of it lands with this proposal, since approving it
approves them.

## Measurement plan

- **What:** the machine code and the instructions of the table's calls,
  and the time of SQL queries that use the table.
- **Tools:** `tessera-bench --base REF --disasm` on every program and
  with `--module`, every commit; `tessera-bench --base REF` with
  counters on `table_int32`, `table_large` and the programs of
  aggregates, for a commit that changes code; `cargo ab measure` on
  the join, group and sort families of `bench/pg`.
- **Thresholds:** those of `docs/measuring.md`. A move must show the
  same machine code. A change fails at more than 1 % instructions, or
  at cycles past their limits. A confirmed excess starts the search of
  "Stop and ask"; one the search does not remove goes to the
  maintainer.

## What we do not do

- **A call for one key, typed keys and values.** For one key at a time
  hashbrown is faster and smaller. A typed layer may come later, over
  the API of batches.
- **Removing records.** Records are only appended. A removal by one
  writer is possible later; by several at once it is a hard problem of
  freeing memory.
- **The promise over damaged bytes in a shared table being built.** It
  would need an atomic read of every byte of a record, at a cost on
  every probe.
- **`no_std`.** A call allocates nothing, so the core could do without
  the standard library, but nobody needs that now. The feature `alloc`
  only leaves the owning table out.
- **32-bit targets.**
- **A crate for spilling.** `tessera-spill` writes Tessera's columns,
  and the files are written by C. Taken up again once the table lives
  apart.
- **A defense against chosen collisions.** The caller gives the hash.
  The README says to use a hash with a secret seed for keys from
  outside.
- **Checksums of records.** Bytes that are damaged but still look like
  a table give a wrong answer without an error. Only a checksum read
  with every record would catch them, at a cost on every probe.
- **A prefetch hint on x86-64** in this change: `simd-primitives-avx2`
  writes it.
- **A full formal proof** with Verus or Creusot. The code would have to
  be written again in their subset. The risk lies in the unsafe code
  and the protocol, which Kani and loom check.
- **Publishing on crates.io.** The crate is not published for now.
  `publish = true` in its manifest only marks its API as a surface of a
  spec, and CI packs it by a dry run; publishing is decided later.

## Risks / Trade-offs

- [A loop of Tessera compiles longer through the cursors or the merge]
  → the machine code of every commit; a search of other forms, then
  the maintainer.
- [The error grows past 8 bytes] → a compile-time assertion.
- [`tessera-core` depends on the table's crate] → written down above.
- [Kani cannot unroll a whole call] → it proves the checks and their
  arithmetic; the matrix and the fuzzer cover whole calls.
- [Fuzzing takes CI time] → a minute a target on a pull request; the
  long run only on request.
- [The model table is a second form of the API] → only under
  `--cfg loom`.
- [This change's spec is written before the requirements it builds on
  are moved] → a task sets it on top of the moved ones before its
  checks start.
- [The branch lives long while `main` moves] → it is rebased onto
  `main` as it moves, and comes into `main` once the crate is whole;
  work on the table in `main` waits for it.

## Migration Plan

The C API gains one call, `tess_table_chunk_used`, and nothing else in
it changes. Two C nodes change inside, as the rule of references asks:
the grouping takes the payloads of its generic states from
`tess_table_payloads`, and the spills of the grouping and the join read
a chunk's used mark through the new call. The
Rust code of the workspace keeps its imports through the exports of
`tessera-core` and `tessera-kernels`. A revert of the merge of the
branch undoes it.
