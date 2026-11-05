# A walk that keeps the columns read behind it, runs of 2026-11-03

The runs that the change `deform-past-varlena` cites: a heap batch's
walk of a row stores the columns its consumers read behind the cursor,
chosen by the walks they save, and reads a value of varying length
with a one-byte header by a short step.

- **Machine**: Apple M5 Pro, macOS 26.6.2. A system process of macOS
  took about one core through most of the runs, and the load reached 4
  to 7 at times: the times come as base and change in turn, each with
  the core's times as its control.
- **PostgreSQL** master (20devel), a release build without assertions.
- **Base**: `06297a9f`. **Change**: `46a8957f`, the change's code; the
  commits after it change tests and documents only. The probes of the
  rule and of the step were commits of their own, never merged; their
  runs below name what each probe changed.

## TPC-H

`cargo tpch compare`, SF 1, base and change in turn, three pairs a
query. A ratio is the change's time with Tessera on over the base's;
below one is faster.

- Serial ([`tpch-serial-pair1.txt`](tpch-serial-pair1.txt),
  [`tpch-serial-pair2.txt`](tpch-serial-pair2.txt), runs
  `tpch-sf1-Z1zhzn` against `tpch-sf1-VY3yPd`, `tpch-sf1-lSwjvu`
  against `tpch-sf1-1QzQjJ`): the geometric mean 0.968 and 0.977, the
  controls 1.013 and 1.004. Q01 0.80 and 0.81, Q12 0.80 and 0.84, Q14
  0.91 and 0.95, Q15 0.92 and 0.96, Q06 0.93 and 0.98; the other
  queries 0.97 to 1.03. Against the core, the geometric mean of on/off
  went from 0.569 and 0.560 to 0.544 and 0.544, Q01's from 0.45 to
  0.37, Q12's from 0.59 and 0.53 to 0.47 and 0.46.
- Two workers ([`tpch-w2-pair1.txt`](tpch-w2-pair1.txt),
  [`tpch-w2-pair2.txt`](tpch-w2-pair2.txt), runs `tpch-sf1-RnwekO`
  against `tpch-sf1-xBqor7`, `tpch-sf1-GE07kn` against
  `tpch-sf1-5UeEnS`): 0.985 and 0.984, the controls 1.008 and 1.004.
  Q01 0.84, Q12 0.90, Q14 0.86 and 0.97.
- The walks from the tuple's start (`Restarted Datums` of `lineitem`'s
  scan, serial): Q01 5.9 M to 4 000, Q12 2.8 M to 1 900; Q06 1.16 M
  and Q14 76 000, which keep nothing, as before.

## The rule that keeps a column

`cargo tpch --queries 1,6,12,14 --pairs 5`, each probe beside the base
in turn, two rounds unless said.

- A column kept when read on a share of the rows a walk reached, the
  approved design, the shares a half, a quarter and an eighth (runs
  `tpch-sf1-IFGKIC`, `ilrDvw`; `nk6q2l`, `sYZ7sI`; `t6Hcp6`,
  `AlyhOO` against `34lSII`, `9DUjMZ`): Q01 0.88 to 0.91, Q06 slower
  at every share. The test of a kept column sat in the plain path of
  every value; once a request chose its path (`FGeDbi`, `PYrbCu`;
  `eElWQy`, `4iYIYO` against `M1BRaS`, `QJb67p`), Q01 0.83 but Q06
  still slower at a quarter: the filter's own column, read on every row
  it is reached, was kept, and its request took the kept path.
- Reads behind the cursor only, a flat share (`OC6tFq`, `uHBwjB`;
  `iQqnVS`, `Fd3dem`; `BY2AeI`, `kSJMaX` against `LAGtyB`,
  `FKXhM5`): at a half Q12's columns were not kept; at an eighth Q12
  took 0.88 and Q06 kept `l_discount` for no gain. The reached rows
  were counted from the cursors a row at a time; the reset took 3.1 %
  of Q14's profile against 0.4 % on the base.
- Reads behind the cursor weighted by the walk's length, both counts
  from the masks, three rounds (`Ppfhb9`, `xTLxKR`, `X7HHtQ` against
  `1PmKBi`, `e3ckcs`, `eLYRRG`): Q01 0.82, Q12 0.84, Q06 and Q14 even.
- The counts on every batch cost the queries that keep nothing: Q03,
  Q08 and Q21 took 2 to 7 % longer with the walk that keeps alone
  (`AXcuTL`, `v9Af3F`, `MdCyhJ` against `6eDp6v`, `KuZAc3`,
  `5a7Jb8`), the same with the step (`OYh86E`, `40wpy8`, `Wt0xeM`),
  their deform counters the base's. Counted on one batch in four
  (`rF4Z9Y`, `GiB9Bb`, `H16CfT` against `7UkPq3`, `tMArhr`,
  `b8V0jb`): Q03, Q08 and Q21 even, Q01 0.82 to 0.85, Q12 0.77 to
  0.85.

## The step for a one-byte header

`cargo tpch --queries 6,14 --pairs 7`, three rounds in turn. The step
with the batch's own walk against the walk that keeps without them
(`h3SXdn`, `zr3e5W`, `809u22` against `NYCSvz`, `iTcFJd`, `iApETF`):
Q06 0.93 to 0.96, the controls 0.95 to 1.01; Q14 even. The own walk
without the step against with it (`kIT4AD`, `615KCv`, `1Db6Ay` against
`JK2qTM`, `5Uq9CH`, `qTZSHe`): 5 to 17 % slower on Q06 and Q14.

## The families

`cargo ab measure`, base `06297a9f` against `46a8957f`, measured again
on 2026-11-05. The runs first cited here measured another worktree's
bench cluster: `bench/pg/run.sh` restarted its own cluster on the port
of its last start and sent its queries to whatever listened on
`PGPORT`, so both sides ran the same foreign build. The runs below come
from the harness that starts the cluster on `PGPORT` and checks the
server it reaches, at the data's scale of 1, a system process of macOS
taking about one core.

- `tax` ([`ab-tax.md`](ab-tax.md)): `mixed_limit` was flagged at 1.033
  and 1.067 by the median against a control of 0.966, `offset_1m` at
  1.035; measured again alone with 101 repetitions in three rounds,
  neither was flagged: `mixed_limit` 1.027 by the least time, 1.043 by
  the median against a control of 1.054
  ([`ab-tax-mixed_limit.md`](ab-tax-mixed_limit.md)), `offset_1m`
  1.008 and 0.982 ([`ab-tax-offset_1m.md`](ab-tax-offset_1m.md)). No
  other case was flagged.
- `win` ([`ab-win.md`](ab-win.md)): the family's 32 cases at 7
  repetitions; `count_all` was flagged at 1.022 by the least time and
  measured again alone with 31 repetitions in three rounds at 1.007 and
  1.000 ([`ab-win-count_all.md`](ab-win-count_all.md)). No other case
  was flagged; the controls moved from 0.87 to 1.04.
