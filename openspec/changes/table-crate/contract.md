# relhash: the architecture contract

`relhash` is a hash table for the engines of relational databases, kept
in memory the caller gives, or in a table that owns its memory for tests
and simple uses. These rules keep it a small box that an engine can take
as it is, and that its checks cover whole. Each rule says what must not
happen and why; how the code is laid out is left to the work.

## Rules

### Free of Tessera
The crate builds without any crate of Tessera and without PostgreSQL,
so that it can be used, and checked, alone.
- **Checked by:** pending

### One way to memory
Every read and write of a table's blocks goes through one layer of
memory, so that the list of the places where the table trusts its own
bytes is complete, and one proof covers every access.
- **Checked by:** pending

### Unsafe code where it is proven
Unsafe code lives only where a proof covers it, and every other module
forbids it, so that no unsafe code goes unproven.
- **Checked by:** pending

### References checked by the crate
Every reference into a table, whether the caller passed it or the call
read it, is checked by the crate before it is followed, and code
outside the crate reads no table's bytes by an offset of its own: it
trusts the record the crate returns. So the checks live, and are
proven, in one place, and none is skipped or done twice.
- **Checked by:** agent review — list every place outside the crate
  that reads a table's index, chunks or records by an offset, and every
  check of a reference there; each one fails the rule

### The masks of rows are the crate's
The masks that choose the rows of a batch are defined once, in the
crate: no other crate of Tessera defines a mask of its own, and the C
API describes the same layout, turned into the crate's type at its
edge, so that one type, with one set of checks, crosses the boundary
of the crate.
- **Checked by:** pending

### No memory taken
A call over the blocks the caller gives allocates nothing, also when it
fails, so that an engine keeps its memory in its own hands.
- **Checked by:** pending

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

### A prefetch hint for x86-64
The probe of a batch asks the processor to fetch the next memory early
on AArch64 alone. Adding the hint for x86-64 touches the crate's own
copy of the hint, and nothing else: no interface, and no code of
Tessera.

### Keys of more kinds
The table keeps a key as words of 64 bits compared bit for bit. Keys of
another kind, such as short text kept whole, touch the crate's source
of keys and its record, and Tessera's reading of keys from its columns:
one interface on each side, and no node.

### Removing records
Records are only appended. Removing them by one writer touches the
crate's calls of one writer and its chunks, which gain free places, and
only the node of Tessera that removes: no other call, and no format of a
spill.
