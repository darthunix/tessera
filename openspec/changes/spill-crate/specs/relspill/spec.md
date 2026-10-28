## ADDED Requirements

### Requirement: A crate of its own
The spill SHALL be the Rust crate `relspill`, which depends on no crate
of Tessera but `relhash`, and SHALL build, pass its tests and pack
alone on targets whose pointers are 64 bits wide; on any other target
it SHALL fail to build, with an error that says why. Its documentation
SHALL be complete by itself: every public item SHALL say what it is
for, what it takes and gives, who owns what and for how long, the
errors it returns, and the rules, the layouts and the limits of the
API.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing
- **THEN** it builds from the packed files and the crates it names,
  of which `relhash` is the only one of Tessera's workspace
- **Verified by:** pending

#### Scenario: The layout of a record comes from relhash
- **WHEN** the crate packs a chunk of records
- **THEN** it reads the layout of a record from `relhash`, so that a
  change of that layout changes the packing or fails to build
- **Verified by:** pending

### Requirement: No input or output
The crate SHALL NOT open, read, write or remove a file. A call that
writes SHALL return the bytes to write and the offset of each in the
file; a call that reads SHALL name the offset and the length it needs,
and take the bytes the caller read.

#### Scenario: A set written and read over memory
- **WHEN** a caller keeps a set's file as bytes in memory, writes the
  ranges the crate returns and reads the ranges it asks for
- **THEN** every block of every partition reads back as it was written
- **Verified by:** pending

### Requirement: No memory taken
A call of the crate SHALL allocate no memory, also when it fails. Every
region a call uses SHALL come from its caller, and a call that needs a
larger region SHALL return how many bytes it needs and change nothing.

#### Scenario: Every call under a counting allocator
- **WHEN** every call of the crate runs under an allocator that counts
- **THEN** the count does not move
- **Verified by:** pending

#### Scenario: A list of blocks that is full
- **WHEN** a block is written to a partition whose list has no room
- **THEN** the call returns the size the list needs, and the set is as
  it was
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

#### Scenario: A block larger than the buffer
- **WHEN** a block is larger than the write buffer
- **THEN** the call returns it as a range of its own, after the
  buffered bytes
- **Verified by:** pending

### Requirement: The spill of a join
The crate SHALL give a machine of a hash join's spill: a build that
goes over its limit sends partitions to disk; the partitions kept in
memory are probed first; each partition on disk is then split to a
level below, joined whole, or joined in pieces over several passes of
its outer rows. A join through the machine SHALL give the rows a join
in memory gives, for every kind of join, under any limit of memory.

#### Scenario: A join under every limit
- **WHEN** a property test joins random rows through the machine under
  a random limit, with files and memory kept by the test
- **THEN** the rows equal those of a join in memory, and the files are
  empty at the end
- **Verified by:** pending

#### Scenario: A partition read in pieces
- **WHEN** a partition read back is larger than the room left and its
  hash has no bits left to split by
- **THEN** it is joined in pieces, and a LEFT, FULL or ANTI join gives
  each outer row once
- **Verified by:** pending

### Requirement: The spill of a grouping
The crate SHALL give a machine of a grouping's spill: groups sent to
disk by partition and merged when read back, a partition split to a
level below when it does not fit, and the rows of aggregates that
cannot merge their states kept on disk and replayed. A grouping through
the machine SHALL give the groups a grouping in memory gives, under any
limit of memory.

#### Scenario: A grouping under every limit
- **WHEN** a property test groups random rows through the machine under
  a random limit
- **THEN** the groups and their states equal those of a grouping in
  memory
- **Verified by:** pending

### Requirement: The runs of a sort
The crate SHALL give a machine of an external sort: when a run goes to
disk, the sets that hold the runs, and the passes that merge them. A
sort through the machine SHALL give the order a sort in memory gives.

#### Scenario: A sort under every limit
- **WHEN** a property test sorts random rows through the machine under
  a random limit
- **THEN** the order equals that of a sort in memory
- **Verified by:** pending

### Requirement: A build by several processes and its rounds
The crate SHALL give a machine of a hash table built by several
processes that may spill, with its phases and its rounds over the
partitions on disk. Any participant MAY join at any phase and leave
early; every row SHALL be found once, every file read once, every
region freed once, and the participants SHALL agree on one split.

#### Scenario: Participants that join at any phase
- **WHEN** a loom model runs two or three participants over a build
  that spills, each joining at any phase
- **THEN** every key is found, every file is read once, and every
  region is freed once
- **Verified by:** pending

#### Scenario: A participant that joins after the split
- **WHEN** a participant joins after the others agreed on a split
- **THEN** it takes the level the others made before it reads the table
- **Verified by:** pending

### Requirement: Any bytes, a bounded read
Whatever bytes a file holds, a read of the crate SHALL return the data
or an error, and SHALL NOT panic, loop without end, or ask for a region
larger than the longest block its caller said it writes. The promise
holds for a file nobody writes while it is read.

#### Scenario: Every damage against every read
- **WHEN** each field of a header, a trailer, a list or a packed body
  of a file is damaged at and around its limits, and every read runs
  over it
- **THEN** each read returns an error naming the rule that refused the
  bytes, or data the damage did not touch
- **Verified by:** pending

#### Scenario: Any bytes as a file
- **WHEN** a fuzzer gives any bytes as a file
- **THEN** no read panics, and none asks for more than the longest
  block
- **Verified by:** pending

### Requirement: Errors of two kinds
An error of the crate SHALL be either a wrong call or damaged bytes,
SHALL name the rule it broke, and SHALL fit in 8 bytes. A machine given
an event it does not expect in its state SHALL return a wrong call and
stay as it was.

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
