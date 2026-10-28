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

### Unsafe code where it is proven
Unsafe code lives only where a proof covers it, and every other module
forbids it, so that no unsafe code goes unproven.
- **Checked by:** pending

### One owner for each rule of a spill
When a spill starts, which partition goes to disk, how a level is
planned, whether a partition splits or is read in pieces, which
partitions are opened, and where each participant goes next are
decided in the crate alone; no node of Tessera decides them or keeps a
copy of their rule, so that each rule is written, tested and proven
once.
- **Checked by:** agent review — list every place in `nodes/` and
  `runtime/` that compares a size, a count or a measure of memory to
  decide one of these, or computes a level's partitions, shift or
  chunk; each one fails the rule

### A small surface
The public API grows only with a reason the change gives, so that what
callers rely on, and what must be proven, stays small.
- **Checked by:** pending

### No cost to Tessera
A hot loop of Tessera compiles no longer through the crate's API than
before the change, so that the box costs its first user nothing.
- **Checked by:** agent review — run `tessera-bench --base <base>
  --disasm` on every benchmark program and with `--module`, and list
  every loop longer than the base's, each with its reason

### SQL stays outside
No rule of SQL, of PostgreSQL's types or of Tessera's executor lives in
the crate, so that a change of those rules never touches it.
- **Checked by:** agent review — list every invariant the change adds
  to the crate, and for each say whether a rule of SQL, of
  PostgreSQL's types or of Tessera's executor decides it; any that one
  does fails the rule

## Forecast

### A spill for a merge join
A merge join that sorts runs larger than memory writes them as the
sort does. It touches the machine of the sort's runs, which gains a
second user, and the node that steps it; no format and no other
machine.

### A host with another manager of temporary files
A database whose temporary files are not PostgreSQL's, such as a fork
with its own manager of work files, writes and reads the ranges the
crate gives through its own calls. It touches only the host's file
calls in `runtime/spill.c`; nothing of the crate.

### A new kind of block
A block of a new kind, such as the states of aggregates written as
they are, touches the format of a block, its packing, the matrix of
damage and the proofs of its bounds, and the machine that writes it;
no other machine and no file of the host.
