## Why

A key that a 64-bit word does not hold goes through its type's
functions: a grouping numbers its values in a dictionary that hashes
and compares them, and a join puts the value's 64-bit hash in its
table. For text and bytea the dictionary already hashes and compares
the bytes itself, where the type's functions read nothing but the
bytes. Every other call goes through fmgr, PostgreSQL's way of calling
a function by its descriptor, and each call of the text and char(n)
functions also looks up the collation and unpacks the value.

Profiles of the backend (macOS `sample`, eight seconds, a serial plan)
show what those calls cost:

- TPC-H Q1 at SF 1 groups 6 M rows by two `char(1)` keys. The
  dictionary takes 1099 of 6806 samples, and 997 of them are the calls:
  `bpchareq`, `hashbpchar`, the collation's lookup, the unpacking and
  their fmgr frames.
- The join of the family `anykey` on a text key (`any_join_text`, 2 M
  rows against 100 000) spends 532 of 6785 samples on the call around
  the hash (`FunctionCall2Coll`, `hashtextextended`, the collation's
  lookup, the unpacking), beside 586 on the hash itself.

## What Changes

- One rule says which keys are hashed and compared by their bytes,
  `tess_key_bytes` in `nodes/key_kind.h`: text and varchar under a
  deterministic collation, bytea, and char(n) under a deterministic
  collation without its trailing blanks. These are the cases where the
  type's functions read the value's bytes and nothing else, so the
  node's own hash of them is the functions' hash, bit for bit.
- The grouping's dictionary takes char(n) by that rule, beside text and
  bytea. Equal char(n) values of other trailing blanks keep their own
  forms, as numeric ones do.
- The join hashes a key of those types by its bytes with
  `hash_bytes_extended` and seed 0, as `hashtextextended`,
  `hashbpcharextended` and `hashbyteaextended` do. Its equality stays
  a join filter, as now.
- A value compressed or stored outside the row still goes to the type's
  functions, and hashes the same, so both forms of one value meet.
- Tests in `types.sql` and `join.sql`, compared with Tessera off.

Not in this change, and placed in the roadmap with these profiles:
numeric keys by their digits (34 % of a grouping by numeric is its
dictionary), the transitions of `bit_or` and of text extremes through
the core's functions, and comparisons computed as values.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: the hashes and the groups are the same, so no promise changes.
The design of `hash-table` says where the node hashes the bytes
itself. The change declares `skip_specs: true`.

## Impact

- Code: `nodes/key_kind.h`, `nodes/agg_keydict.c`, `nodes/agg_node.h`,
  `nodes/hashjoin.c`, `nodes/hashjoin.h`, `nodes/hashjoin_begin.c`.
- Tests: `test/sql/types.sql`, `test/sql/join.sql`.
- Documents: `docs/nodes.md`, `openspec/specs/hash-table/design.md`.
- Runs: TPC-H Q1 serial and with two workers; `anykey` with its text
  joins and groupings.
