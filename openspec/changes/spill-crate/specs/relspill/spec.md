## ADDED Requirements

### Requirement: A crate of its own
The spill SHALL be the Rust crate `relspill`, which depends on no crate
of Tessera except `relhash`. It SHALL build, pass its tests and be
packaged with `relhash` alone, on targets with 64-bit pointers; on any
other target it SHALL fail to build, with an error that says why. Its
documentation SHALL be complete by itself: every public item SHALL say
what it is for, what it takes and gives, who owns what and for how
long, and the errors it returns, and the documentation SHALL hold the
rules, the layouts and the limits of the API.

#### Scenario: The crate packaged alone
- **WHEN** the crate is packaged with `relhash`, as for publishing
- **THEN** it builds from the packaged files and the crates they name,
  and `relhash` is the only one of them from Tessera's workspace
- **Verified by:** pending

#### Scenario: A 32-bit target
- **WHEN** the crate is built for a target with 32-bit pointers
- **THEN** the build fails with an error that says the crate needs
  64-bit pointers
- **Verified by:** pending

#### Scenario: Records packed as relhash lays them out
- **WHEN** the crate packs and unpacks chunks of records that a
  `relhash` table wrote
- **THEN** each record reads back equal to the table's, field by field
- **Verified by:** pending

#### Scenario: Every public item documented
- **WHEN** the documentation of the crate is built
- **THEN** no public item lacks documentation, and there is no warning
- **Verified by:** pending

### Requirement: No input or output
The crate SHALL NOT open, read, write or remove a file. A call that
writes SHALL return the bytes to write and the offset of each in the
file. A call that reads SHALL name the offset and the length it needs,
and SHALL take the bytes the caller read. When the caller reports an
action it could not finish, the set or the machine SHALL accept only a
free or a reset afterwards, and SHALL refuse any other call as a wrong
call.

#### Scenario: A set written and read in memory
- **WHEN** a caller keeps the file of a set as bytes in memory, writes
  the ranges the crate returns, and reads the ranges it asks for
- **THEN** every block of every partition reads back as it was written
- **Verified by:** pending

#### Scenario: A write that fails half way
- **WHEN** the caller reports a write that it finished only in part
- **THEN** the set refuses every call but a free or a reset, and a set
  that was reset writes and reads like a new one
- **Verified by:** pending

### Requirement: No memory allocated
A call of the crate SHALL allocate no memory, even when it fails. Every
region a call uses SHALL come from its caller. A call that cannot start
without a larger region SHALL return how many bytes it needs and change
nothing. A call that appends rows MAY first append the rows that fit;
it SHALL then say which rows it appended and how many bytes the next
row needs.

#### Scenario: Every call under a counting allocator
- **WHEN** every call of the crate runs under an allocator that counts
  allocations
- **THEN** the count does not change
- **Verified by:** pending

#### Scenario: A full block list
- **WHEN** a block is written to a partition whose block list has no
  room left
- **THEN** the call returns the size the list needs, and the set is
  unchanged
- **Verified by:** pending

#### Scenario: Rows that fit in part
- **WHEN** two rows are appended to a chunk of columns whose region of
  values holds only the first
- **THEN** the first row is appended, the call says so, and it returns
  the bytes the second row needs
- **Verified by:** pending

### Requirement: The file of a set
The crate SHALL own the whole format of the file of a set: the blocks,
the block list of each partition, and the trailer of a file that other
participants read. It SHALL keep the write buffer. It SHALL check every
list, entry and trailer it reads before it asks for the bytes they
point to.

#### Scenario: A shared file read by another participant
- **WHEN** a participant finishes its file, and another participant
  reads one of its partitions
- **THEN** the reader gets the blocks of that partition in the order
  they were written, and no block of another partition
- **Verified by:** pending

#### Scenario: A partition dropped before the set is finished
- **WHEN** a participant drops a partition of a shared set before it
  finishes the set
- **THEN** the call is a wrong call, and the set is unchanged
- **Verified by:** pending

### Requirement: The spill of a join
The crate SHALL provide a state machine for the spill of a hash join.
A build that goes over its memory limit sends partitions to disk. The
partitions kept in memory are probed first. Then each partition on disk
is split into a level below, joined whole, or joined in pieces over
several passes of its outer rows. With a host that does what its
actions say, the machine SHALL give the rows that a join in memory
gives, at any memory limit: every matching pair once; a row of a SEMI
join once; an outer row without a match, in a LEFT, FULL or ANTI join,
once, after the last piece of its partition; and an inner row without
a match, in a RIGHT or FULL join, once.

#### Scenario: A join under any memory limit
- **WHEN** a property test joins random rows through the machine under
  a random memory limit, with the files and the memory kept by the test
- **THEN** the pairs equal those of a join in memory, each as many
  times, and the files are empty at the end
- **Verified by:** pending

#### Scenario: A partition joined in pieces
- **WHEN** a partition read back does not fit in the room left, and its
  hash has no bits left or its keys do not split
- **THEN** it is joined in pieces: each matching pair comes once, and
  an outer row without a match comes once, after the last piece
- **Verified by:** pending

### Requirement: The spill of a grouping
The crate SHALL provide a state machine for the spill of a grouping.
Groups are sent to disk by partition and merged when they are read
back. A partition that does not fit is split into a level below. The
rows of aggregates that cannot merge their states are kept on disk and
replayed. With a host that does what its actions say, the machine SHALL
give the groups that a grouping in memory gives, at any memory limit.

#### Scenario: A grouping under any memory limit
- **WHEN** a property test groups random rows through the machine under
  a random memory limit
- **THEN** the groups and their states equal those of a grouping in
  memory
- **Verified by:** pending

### Requirement: The runs of a sort
The crate SHALL provide a state machine for an external sort: it says
when a run goes to disk, which sets hold the runs, and which passes
merge them. With a host that does what its actions say, the machine
SHALL give every item once, each no greater than the next by the host's
comparison; items that compare equal MAY come in any order. A sort that
is read again SHALL read its runs again, without sorting its input
again.

#### Scenario: A sort under any memory limit
- **WHEN** a property test sorts random rows through the machine under
  a random memory limit
- **THEN** the items are those given, each once, and each is no
  greater than the next
- **Verified by:** pending

#### Scenario: A sort read again
- **WHEN** the host asks to read the sorted items again from the start
- **THEN** the machine reads its runs again, and the items come in the
  same order
- **Verified by:** pending

### Requirement: A build by several processes and its rounds
The crate SHALL provide a state machine for a hash table that several
processes build together and that may spill, with its phases and its
rounds over the partitions on disk. A participant MAY attach at any
phase of the build, or to a round in progress, and MAY leave early. A
participant that comes after the in-memory part of the build is freed
SHALL find no work. Each partition on disk SHALL be joined once, by one
participant alone or by all of them together in its round. Every
region SHALL be freed once, and the participants SHALL agree on one
split.

#### Scenario: Participants that attach at any phase
- **WHEN** a loom model runs two or three participants over a build
  that spills, each attaching at any phase
- **THEN** every key is found, every partition on disk is joined once,
  and every region is freed once
- **Verified by:** pending

#### Scenario: A participant that attaches after the split
- **WHEN** a participant attaches after the others agreed on a split
- **THEN** it takes the level the others made before it reads the
  table
- **Verified by:** pending

#### Scenario: A participant that comes after the build
- **WHEN** a participant attaches after the in-memory part of the build
  is freed
- **THEN** the machine tells it that it has no work, and the rounds of
  the others go on
- **Verified by:** pending

#### Scenario: A partition taken alone
- **WHEN** one participant takes a whole partition on disk
- **THEN** it joins the partition through its join machine, in pieces
  if it does not fit, and no other participant joins it
- **Verified by:** pending

### Requirement: Any bytes, a bounded read
Whatever bytes a file holds, a read of the crate SHALL return the data
or an error. It SHALL NOT panic, loop without end, or ask for more than
its caller allows: one block of the kind it reads, with its header and
the slack of packing, or one page of a list, and never more than the
file holds. A read SHALL give its caller a chunk only of the kind and
the shape the caller expects. The promise holds for a file that nobody
writes while it is read. Bytes damaged into other bytes that pass every
check read as other data, since a block has no checksum.

#### Scenario: Every damage against every read
- **WHEN** each field of a header, a trailer, a list or a packed body
  of a file is damaged at and around its limits, and every read runs
  over the file
- **THEN** each read returns an error that names the rule that refused
  the bytes, or blocks that pass every check
- **Verified by:** pending

#### Scenario: Any bytes as a file
- **WHEN** a fuzzer gives any bytes as a file
- **THEN** no read panics, and none asks for more than its bounds
- **Verified by:** pending

#### Scenario: A chunk of another shape
- **WHEN** a block of another kind, or a chunk of columns of another
  shape, is read where the caller expects a given kind and shape
- **THEN** the read is refused as damaged bytes
- **Verified by:** pending

### Requirement: Errors of two kinds
An error of the crate SHALL be either a wrong call or damaged bytes,
SHALL name the rule it broke, and SHALL fit in 8 bytes. A machine given
an event it does not expect in its state SHALL return a wrong call and
stay as it was. A call that needs more memory, and an action the host
could not do, SHALL NOT be errors of the crate.

#### Scenario: The kind follows what was wrong
- **WHEN** one call is given a partition past the level's count, and
  another reads a block whose header is damaged
- **THEN** the first is a wrong call and the second is damaged bytes,
  and each names its rule
- **Verified by:** pending

#### Scenario: An event out of turn
- **WHEN** a machine is given an event its state does not expect
- **THEN** it returns a wrong call, and its next step goes as if the
  event had not come
- **Verified by:** pending
