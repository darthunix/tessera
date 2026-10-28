# relspill: the architecture contract

`relspill` is the spill of a hash join, a grouping and a sort for the
engines of relational databases: the format of the files, the words
several processes share, and the protocol as state machines, over
`relhash`'s tables. Its host gives it memory and does its input and
output. These rules keep it a small box that an engine can take as it
is, and that its checks cover whole. Each rule says what must not
happen and why; how the code is laid out is left to the work.

## Rules

### Free of Tessera
The crate builds without any crate of Tessera but `relhash`, and
without PostgreSQL, so that it can be used, and checked, alone.
- **Checked by:** pending

### No input or output
The crate opens, reads, writes and removes no file, and waits for
nothing: its host does all of it and tells the crate what happened, so
that no error of the host jumps over the crate's frames, and a test, a
fuzzer, a proof or a model runs the crate whole over memory.
- **Checked by:** pending

### No memory taken
A call of the crate allocates nothing, also when it fails, so that the
memory of a spill is counted where its host counts memory.
- **Checked by:** pending

### One way to bytes read back
Every read of bytes that came from a file goes through one layer, so
that the list of the places where the crate trusts such bytes is
complete, and one proof covers every read.
- **Checked by:** pending

### No unsafe code
The crate has no unsafe code: the memory of a spill, the words in
shared memory among it, comes to it as safe slices its host makes, so
that the compiler alone keeps it sound and no proof of it is owed.
- **Checked by:** pending

### One owner for each rule of a spill
When a spill starts, which partition goes to disk, how a level is
planned, whether a partition splits or is read in pieces, which
partitions are opened, and where each participant goes next are
decided in the crate alone; no node of Tessera decides them or keeps a
copy of their rule, so that each rule is written, tested and proven
once.
- **Checked by:** agent review — list every place in `nodes/`, their
  planners included, and in `runtime/` that compares a size, a count
  or a measure of memory, or tests a flag of a partition or a phase, to
  decide one of these, or computes a level's partitions, shift, chunk
  or bits of a hash; each one fails the rule

### A small surface
The public API grows only with a reason the change gives, so that what
callers rely on, and what must be proven, stays small.
- **Checked by:** pending

### No cost to Tessera
A hot loop of Tessera compiles no longer through the crate's API than
before the change, so that the box costs its first user nothing.
- **Checked by:** agent review — run `tessera-bench --base <base>
  --disasm` on every benchmark program and with `--module nodes` and
  `--module kernels`, and list every loop longer than the base's, each
  with its reason

### SQL stays outside
No rule of SQL, of PostgreSQL's types or of Tessera's executor lives in
the crate, so that a change of those rules never touches it.
- **Checked by:** agent review — list every invariant the change adds
  to the crate, and for each say whether a rule of SQL, of
  PostgreSQL's types or of Tessera's executor decides it; any that one
  does fails the rule

## Forecast

### A merge join over sorted runs
A merge join whose inputs do not fit in memory sorts them through the
runs of a sort. It touches the machine of the sort's runs, which gains
a second user, and the node that steps it; no format and no other
machine. Keeping a group of equal keys on disk to read it again is
another problem, which this forecast does not cover.

### A host with another manager of temporary files
A database whose temporary files are not PostgreSQL's, such as a fork
with its own manager of work files, writes and reads the ranges the
crate gives through its own calls. It touches only the host's file
calls in `runtime/spill.c`; nothing of the crate. A host that is not
PostgreSQL at all also writes its own ways to give memory, to wait at
a barrier, to give rows and to apply the actions to its operators; it
touches nothing of the crate either.

### A new kind of block
A block of a new kind, such as the states of aggregates written as
they are, touches the format of a block, its packing, the matrix of
damage and the proofs of its bounds, the machine that writes it, and
the host's code that writes and reads such states; no other machine.

### A change of the record of relhash
A record of `relhash` laid out anew touches the packing of records, its
proofs and its rows of the matrix of damage; no machine and nothing of
the host.

### A grouping by several processes that spills
A grouping whose processes share one table and spill it takes the
machine of a build by several processes as its second user. It touches
that machine's parameters, the machine of the grouping, and the node;
no format.
