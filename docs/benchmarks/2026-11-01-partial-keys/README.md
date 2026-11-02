# Partial grouping by dictionary keys, runs of 2026-11-01

The runs that the change `partial-dictionary-keys` cites: the partial
stage of `TessAgg` under a `Gather` for keys that go through a
dictionary (text, `char(n)`, numeric), against the base `ee33f2e4`
that kept such keys off it.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power. Other sessions
  of work shared it; the columns of times without Tessera show how far
  the machine moved.
- **PostgreSQL** master (20devel), a release build without assertions;
  `shared_buffers` 2 GB.
- **Tessera**: the base `ee33f2e4`, and the change at `27effe46`, its
  state before the outside reviews, on the same base. The reviews
  changed when a table is too full and how the dictionaries share their
  room; Q1's four groups reach neither, and the family's groupings are
  serial. The answers were checked again at the change's last commit,
  rebased on `eaf000a8`: `cargo tpch check --workers 2`
  (`tpch-sf1-eApTRs`) and with `--work-mem 4MB` (`tpch-sf1-dgenYG`),
  every answer the published one but Q17 and Q20, out of time.

## TPC-H with two workers

[`participation-w2.diff`](participation-w2.diff) compares the Tessera
nodes of every query of `cargo tpch check --workers 2` at SF 1: only
Q1, which groups `lineitem` by two `char(1)` keys, changes its plan.
The base groups in the core's `Partial HashAggregate` over the node's
scan; the branch keeps the whole stack, the node's partial grouping in
each participant, `TessGather` and the node's final grouping. Every
answer is the published one on both sides, but Q17 and Q20, which run
past the time limit without indexes on both.

[`tpch-q1-w2.txt`](tpch-q1-w2.txt) is `cargo tpch compare` of Q1 and of
Q6, the control, 11 pairs a query (runs `tpch-sf1-qAvr06` of the base,
`tpch-sf1-TdXv16` of the branch). Q1 with Tessera takes 307.8 ms
against 632.5, 0.487 of the base's time; with the base Tessera made
Q1 slower than the core alone (1.079), with the branch 0.526 of it. Q6
moves by 0.974, its time without Tessera by 0.987.

## The family anykey with two workers

[`ab-anykey-w2.md`](ab-anykey-w2.md) is `cargo ab measure --base
ee33f2e4 --family anykey --workers 2` on the bench cluster at scale 1.
At 500 000 rows the planner keeps every grouping of the family serial,
on both sides: the plans are the same, so the family is the control of
the executor's part of the change. Three cases were flagged slower, by 3
to 5 % at their minimum.
[`ab-anykey-w2-flagged.md`](ab-anykey-w2-flagged.md) measures them again
alone, 101 repetitions, three rounds a side: by the minimum all three
are 0.996 to 0.998, and none is flagged; by the median two are 6 to 7 %
higher, one of them a join the change does not reach, so the medians
follow the shared machine. The header of that report reads battery
power, sampled once when it was written; each of its six runs recorded
AC power.
