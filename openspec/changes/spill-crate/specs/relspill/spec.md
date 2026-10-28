## ADDED Requirements

### Requirement: A crate of its own
The spill SHALL be the Rust crate `relspill`, which depends on no crate
of Tessera but `relhash`. It SHALL build, pass its tests and pack with
`relhash` alone, on targets whose pointers are 64 bits wide; on any
other target it SHALL fail to build, with an error that says why. Its
documentation SHALL be complete by itself: every public item SHALL say
what it is for, what it takes and gives, who owns what and for how
long, the errors it returns, and the rules, the layouts and the limits
of the API.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed with `relhash`, as for publishing
- **THEN** it builds from the packed files and the crates they name,
  of which `relhash` is the only one of Tessera's workspace
- **Verified by:** pending

#### Scenario: A target of 32 bits
- **WHEN** the crate is built for a target whose pointers are 32 bits
  wide
- **THEN** the build fails with an error that says the crate needs
  64-bit pointers
- **Verified by:** pending

#### Scenario: Records packed as relhash lays them out
- **WHEN** the crate packs and unpacks chunks of records that a table
  of `relhash` wrote
- **THEN** each record reads back equal to the table's, field by field
- **Verified by:** pending

#### Scenario: Every public item documented
- **WHEN** the documentation of the crate is built
- **THEN** no public item lacks its documentation, and no warning is
  given
- **Verified by:** pending

### Requirement: No input or output
The crate SHALL NOT open, read, write or remove a file. A call that
writes SHALL return the bytes to write and the offset of each in the
file; a call that reads SHALL name the offset and the length it needs,
and take the bytes the caller read. The caller SHALL report an action
it could not finish; the set or the machine SHALL then take no call but
a free or a reset, and SHALL refuse any other as a wrong call.

#### Scenario: A set written and read over memory
- **WHEN** a caller keeps a set's file as bytes in memory, writes the
  ranges the crate returns and reads the ranges it asks for
- **THEN** every block of every partition reads back as it was written
- **Verified by:** pending

#### Scenario: A write that fails half way
- **WHEN** the caller reports a write it finished only in part
- **THEN** the set refuses every call but a free or a reset, and a set
  reset writes and reads as a new one
- **Verified by:** pending

### Requirement: No memory taken
A call of the crate SHALL allocate no memory, also when it fails. Every
region a call uses SHALL come from its caller. A call that cannot start
without a larger region SHALL return how many bytes it needs and change
nothing. A call that appends rows MAY first append the rows that fit;
it SHALL then say which it appended and what the next row needs.

#### Scenario: Every call under a counting allocator
- **WHEN** every call of the crate runs under an allocator that counts
- **THEN** the count does not move
- **Verified by:** pending

#### Scenario: A list of blocks that is full
- **WHEN** a block is written to a partition whose list has no room
- **THEN** the call returns the size the list needs, and the set is as
  it was
- **Verified by:** pending

#### Scenario: Rows that fit in part
- **WHEN** two rows are appended to a chunk of columns whose region of
  values holds only the first
- **THEN** the first is appended, the call says so, and it returns the
  bytes the second needs
- **Verified by:** pending

### Requirement: The file of a set
The crate SHALL own the whole format of a set's file: the blocks, the
lists of each partition's blocks, and the trailer of a file that
other participants read. It SHALL keep the write buffer, and SHALL
check every list, entry and trailer it reads before it asks for the
bytes they name.

#### Scenario: A shared file read by another participant
- **WHEN** a participant finishes its file and another reads one of its
  partitions
- **THEN** the reader gets that partition's blocks in the order they
  were written, and none of another partition
- **Verified by:** pending

#### Scenario: A partition dropped before the set is finished
- **WHEN** a participant drops a partition of a shared set before it
  finishes the set
- **THEN** the call is a wrong call, and the set is as it was
- **Verified by:** pending

### Requirement: The spill of a join
The crate SHALL give a machine of a hash join's spill: a build that
goes over its limit sends partitions to disk; the partitions kept in
memory are probed first; each partition on disk is then split to a
level below, joined whole, or joined in pieces over several passes of
its outer rows. Driven by a host that does what its actions say, the
machine SHALL lead to the rows a join in memory gives, at any limit of
memory: every matching pair once, a row of a SEMI join once, and an
outer row of a LEFT, FULL or ANTI join without a pair once, after the
last piece of its partition; an inner row of a RIGHT or FULL join
without a pair once.

#### Scenario: A join under every limit
- **WHEN** a property test joins random rows through the machine under
  a random limit, with files and memory kept by the test
- **THEN** the pairs equal those of a join in memory, each as many
  times, and the files are empty at the end
- **Verified by:** pending

#### Scenario: A partition read in pieces
- **WHEN** a partition read back does not fit the room left, and its
  hash has no bits left or its keys do not split
- **THEN** it is joined in pieces; each matching pair comes once, and
  an outer row without a pair comes once, after the last piece
- **Verified by:** pending

### Requirement: The spill of a grouping
The crate SHALL give a machine of a grouping's spill: groups sent to
disk by partition and merged when read back, a partition split to a
level below when it does not fit, and the rows of aggregates that
cannot merge their states kept on disk and replayed. Driven by a host
that does what its actions say, the machine SHALL lead to the groups a
grouping in memory gives, at any limit of memory.

#### Scenario: A grouping under every limit
- **WHEN** a property test groups random rows through the machine under
  a random limit
- **THEN** the groups and their states equal those of a grouping in
  memory
- **Verified by:** pending

### Requirement: The runs of a sort
The crate SHALL give a machine of an external sort: when a run goes to
disk, the sets that hold the runs, and the passes that merge them.
Driven by a host that does what its actions say, the machine SHALL
lead to every item once, each no greater than the next by the host's
comparison; items that compare equal MAY come in any order. A sort read
again SHALL read its runs again, without sorting its input again.

#### Scenario: A sort under every limit
- **WHEN** a property test sorts random rows through the machine under
  a random limit
- **THEN** the items are those given, each once, and each is no
  greater than the next
- **Verified by:** pending

#### Scenario: A sort read again
- **WHEN** the host asks to read the sorted items again from the start
- **THEN** the machine reads its runs again and the items come in the
  same order
- **Verified by:** pending

### Requirement: A build by several processes and its rounds
The crate SHALL give a machine of a hash table built by several
processes that may spill, with its phases and its rounds over the
partitions on disk. A participant MAY attach at any phase of the build,
or to a round in progress, and MAY leave early; one that comes after
the part of the build in memory is freed SHALL find no work. Each
partition on disk SHALL be joined once, by one participant alone or by
all together in its round; every region SHALL be freed once, and the
participants SHALL agree on one split.

#### Scenario: Participants that attach at any phase
- **WHEN** a loom model runs two or three participants over a build
  that spills, each attaching at any phase
- **THEN** every key is found, every partition on disk is joined once,
  and every region is freed once
- **Verified by:** pending

#### Scenario: A participant that attaches after the split
- **WHEN** a participant attaches after the others agreed on a split
- **THEN** it takes the level the others made before it reads the table
- **Verified by:** pending

#### Scenario: A participant that comes after the build
- **WHEN** a participant attaches after the part of the build in memory
  is freed
- **THEN** the machine tells it that it has no work, and the rounds of
  the others go on
- **Verified by:** pending

#### Scenario: A partition taken alone
- **WHEN** one participant takes a partition on disk whole
- **THEN** it joins the partition through its join machine, in pieces
  if it does not fit, and no other participant joins it
- **Verified by:** pending

### Requirement: Any bytes, a bounded read
Whatever bytes a file holds, a read of the crate SHALL return the data
or an error, and SHALL NOT panic, loop without end, or ask for a region
past the bounds its caller gave: one block of the kind it reads, with
its header and the slack of packing, or one page of a list, and never
more than the file holds. A read SHALL give its caller a chunk only of
the kind and the shape the caller expects. The promise holds for a file
nobody writes while it is read; bytes damaged into others that pass
every check read as other data, since a block has no checksum.

#### Scenario: Every damage against every read
- **WHEN** each field of a header, a trailer, a list or a packed body
  of a file is damaged at and around its limits, and every read runs
  over it
- **THEN** each read returns an error naming the rule that refused the
  bytes, or blocks that pass every check
- **Verified by:** pending

#### Scenario: Any bytes as a file
- **WHEN** a fuzzer gives any bytes as a file
- **THEN** no read panics, and none asks for more than its bounds
- **Verified by:** pending

#### Scenario: A chunk of another shape
- **WHEN** a block of another kind, or a chunk of columns of another
  shape, is read where the caller expects a kind and a shape
- **THEN** the read is refused as damaged bytes
- **Verified by:** pending

### Requirement: Errors of two kinds
An error of the crate SHALL be either a wrong call or damaged bytes,
SHALL name the rule it broke, and SHALL fit in 8 bytes. A machine given
an event it does not expect in its state SHALL return a wrong call and
stay as it was. A call that needs more memory, and an action the host
could not do, SHALL NOT be errors of the crate.

#### Scenario: The kind follows what was wrong
- **WHEN** a call is given a partition past the level's count, and
  another reads a block whose header is damaged
- **THEN** the first is a wrong call and the second is damaged bytes,
  each naming its rule
- **Verified by:** pending

#### Scenario: An event out of turn
- **WHEN** a machine is given an event its state does not expect
- **THEN** it returns a wrong call, and its next step is as if the
  event had not come
- **Verified by:** pending
