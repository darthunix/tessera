## Context

TessAgg's dictionary (`nodes/agg_keydict.c`) numbers a key's values by
a hash table of its own; TessHashJoin (`nodes/hashjoin.c`) puts a key's
64-bit hash in the table and keeps the key's equality as a join filter.
Both call the type's functions for every row, but where the dictionary
already reads text by its bytes (`dict->bytewise`). It meant to read
bytea so too, but knew bytea by `hashvarlena`, which PostgreSQL 18
replaced in bytea's operator family with `hashbytea`, so on master
bytea went through the functions as well.

PostgreSQL's functions read only the bytes in these cases. Under a
deterministic collation, `hashtext` is `hash_any` of the bytes and
`texteq` compares them; `hashbpchar` is `hash_any` of the bytes before
the trailing blanks (`bcTruelen`) and `bpchareq` compares those;
`hashbytea` (`hashvarlena` before PostgreSQL 18) is `hash_any` of a
bytea's bytes. The extended hashes are
`hash_any_extended` of the same bytes with the seed.

## Goals / Non-Goals

**Goals:**

- No call through fmgr for a char(n) key of a grouping and a text,
  char(n) or bytea key of a join, where the value's bytes are at hand.
- The same hashes and the same groups and pairs as now.

**Non-Goals:**

- A faster hash than PostgreSQL's: a compressed value still hashes by
  the type's function, and the two must agree.
- numeric, uuid, name and other types; sorts; generic aggregates.

## Decisions

### One rule of a key's bytes

`tess_key_bytes(hashproc, equality, collation)` names the form: none,
the whole value, or the value without its trailing blanks. It maps the
hash function, the 32-bit or the extended one, so the dictionary and
the join ask the same question; the dictionary also passes its
equality, since it compares by it, and a join checks its equality
apart. `tess_key_value_bytes` gives a value's bytes, or false for a
compressed or external one. The rule lives beside the key kinds in
`key_kind.h`, which already is the one place of how a key is hashed.

### The forms of char(n)

The dictionary's flag that said "hash by bytes" also said "equal values
are equal images", and so turned off the forms that keep each group's
first value. That holds for text and bytea, not for char(n): `'o'` and
`'o  '` are equal and differ in bytes. The form is now an enum, and
only the whole value means equal images.

## Risks / Trade-offs

- A hash computed two ways must agree. The native one equals the
  function's by construction in these cases; the tests join and group
  the same value stored compressed and plain, and a test fails when the
  trim of char(n) is removed.
- Encodings: `bcTruelen` trims the byte 0x20, a unit of every server
  encoding, and so does the node.

## Measurement

Before the code, the profiles of the proposal. After it, on mains
power: `cargo tpch --queries 1 --workers 0` and `--workers 2` against
the base, compared with `cargo tpch compare`; `cargo ab measure
--family anykey` for `any_join_text`, `any_join_two`, `any_join_semi`
and the text groupings as controls; the profiles again, to see the
calls gone. A case slower beyond the tool's threshold stops the work.
