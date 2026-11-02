# Keys hashed by their bytes, runs of 2026-11-02

The profiles and the times that the change `native-key-hashes` cites:
a grouping's char(n) key and a join's text, char(n) and bytea key
hashed and compared by their bytes, without a call of the type's
functions through fmgr.

- **Machine**: Apple M5 Pro, macOS 26.6.2, **on battery** for every
  run: the times are a guide, not a measurement by the rules of
  `docs/measuring.md`, which ask for mains power. Between two runs of
  the same code a case the change does not reach (`any_join_semi`)
  moved from 0.867 to 1.022 by its median, so differences of a few
  percent are within the noise. Other sessions of work shared the
  machine.
- **PostgreSQL** master (20devel), a release build without assertions;
  `shared_buffers` 2 GB; TPC-H at SF 1 and the bench cluster at scale
  1, serial plans.

## Profiles

`profile-*.txt` are the self times of a backend's functions, macOS
`sample` for eight seconds while it ran one query in a loop, the most
sampled first. Before is the base of the change; after, its first
version, before the reviews mapped bytea's own hash functions and the
dictionary's equality was made inline again.

- TPC-H Q1 (`profile-q1.txt`, `profile-q1-after.txt`): the dictionary
  took 1099 of 6806 samples, 997 of them in `bpchareq`, `hashbpchar`,
  the collation's lookup, the unpacking and their fmgr frames; after,
  666 of 6839, none in those functions.
- The join of `any_join_text` (`profile-join.txt`,
  `profile-join-after.txt`): `FunctionCall2Coll`, `hashtextextended`,
  the collation's lookup and the unpacking took 532 of 6785 samples
  beside 586 of `hash_bytes_extended`; after, none of the call, the
  hash inline.

## Times

[`tpch-q1-serial.txt`](tpch-q1-serial.txt) is `cargo tpch compare` of
Q1 and of Q6, the control, 7 pairs a query, serial (runs
`tpch-sf1-VDIR4T` of the base `7978d70a`, `tpch-sf1-4S3xub` of the
change). Q1 takes 0.936 of the base's time [0.914–0.954], while the
times without Tessera moved by 1.029 and Q6 by 1.054: the machine was
slower in the second run. A first run of the change before the inline
fix (`tpch-sf1-yz3bay`) gave 0.910 [0.890–0.921] with the control at
1.007.

[`ab-anykey.md`](ab-anykey.md) is `cargo ab measure --family anykey`
of the change's first version against `7978d70a` (run `ab-v6M0A3`,
11 repetitions, two rounds): it flagged `any_text_few`, a grouping by
text of 99 values that the change should not touch, 1.071 by the
minimum. Two more rounds of 51 repetitions gave 1.066. The machine code
(`tessera-bench --disasm --module nodes`) showed the dictionary's
equality, longer with char(n), out of the table's loops in a function
of its own; made inline again, the case measured 1.025 by the minimum
and 1.016 by the median ([`ab-anykey-inline.md`](ab-anykey-inline.md),
run `ab-9tzyWd`, 51 repetitions, three rounds), within the noise.
