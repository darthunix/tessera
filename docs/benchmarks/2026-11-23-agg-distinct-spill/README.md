# agg-distinct-spill: the measurements

The runs of the measurement plan of the OpenSpec change
`agg-distinct-spill`: a grouping's DISTINCT set that sends its pairs of
group and value to disk past `hash_mem`. Every run counts the cycles
that [pg_pmu](https://github.com/darthunix/pg_pmu) counts, with the
instructions beside them; no run takes times, at the maintainer's word.

- Machine: a MacBook Pro, Apple M5 Pro, on mains power, macOS, which
  counts the kernel's work too.
- `bench/pg` and `cargo ab`: PostgreSQL 20devel built for release,
  without assertions; Tessera 8e126650, the base 75fb4ba4, the commit
  `main` was at when the change began.
- ClickBench: PostgreSQL 19rc2 with pg_duckdb 1.2.0, Tessera of a local
  merge of the branch `parquet-fdw` (04b491b0) with this one
  (8e126650), never pushed.
- 8e126650 is a commit of the branch before its last review, which the
  reports name. The review's fixes count the groups' index in floating
  point and at 1024 buckets at least, which changes the estimate of no
  case here, and move tests and documents.

## 1. The family spill, Tessera on against the core

`METRIC=cycles bench/pg/run.sh measure spill`, one `work_mem` a run
(`CASES='^distinct_[a-z]+_4mb$'` and so on), three readings a case in
each mode. Each figure is the median of Tessera on's cycles over the
median of the core's; below 1 Tessera takes fewer. The core's counts
carry `EXPLAIN ANALYZE`'s counting of the rows of a row-wise node one
by one, so a ratio is a little low.

| case | 4 MB | 16 MB | 64 MB |
|---|--:|--:|--:|
| distinct_big | 0.257 | 0.272 | 0.223 |
| distinct_generic | 0.293 | 0.315 | 0.261 |
| distinct_two | 0.328 | 0.337 | 0.267 |
| distinct_keys | 0.180 | 0.191 | 0.153 |
| distinct_half | 1.053 | 0.590 | 0.711 |
| distinct_thought | – | 0.135 | 0.146 |

- distinct_half at 4 MB is the core's plan in both modes: its 300 000
  groups pass half of `hash_mem` by the estimate. Tessera gives it the
  scan alone, under the core's sort; the 5 % is that scan's tax, and
  `main` runs the same plan with the same counts (section 2). The
  plan's threshold, a ratio below 1 in every case, misses here; the
  maintainer chose to keep the case and the figure.
- Memory, the node's `Memory Usage` against `hash_mem` (twice
  `work_mem`): within it in every case but two at 4 MB, of 8192 kB:
  distinct_generic takes 8329 kB and two sets 9930. A set's table may
  take a chunk of up to 1 MB in the batch that passes seven eighths,
  and each set writes a level of its own, 32 chunks of 8 kB and a
  buffer of 256 kB: the spec allows both.
- Disk: at 4 MB distinct_big writes 52 MB of pairs, the core's sort
  102 MB; at 16 MB 39 MB and 102 MB. At 64 MB the pairs fit, but
  distinct_half's 3 M pairs, which pass it and write 7 MB.
- The first runs, on the code before the fixes of section 2, are in
  `spill/first-*`. They found distinct_half at 16 MB at 34 689 kB of a
  `hash_mem` of 32 768: its groups took about 16 449 kB, as the same
  rows grouped without DISTINCT show, while the planner's estimate
  counted 11 294 kB and no index. The estimate now counts the
  index the node makes, two buckets of 4 bytes a group, a power of two
  of them, and the node takes 30 593 kB.

## 2. The path that fits, against main

`cargo ab measure --base 75fb4ba4 --family spill --counters`, the cases
at 64 MB, where the pairs fit, distinct_thought at 16 MB and
distinct_half at 4 MB, both sides alternating; the control is the core
without Tessera, which the change does not touch.

| case | instructions | cycles, 3 rounds |
|---|--:|--:|
| distinct_big_64mb | 1.006 | 1.002 |
| distinct_two_64mb | 1.008 | 1.001 |
| distinct_keys_64mb | 0.951 | 0.979 |
| distinct_thought_16mb | 0.947 | – |
| distinct_thought_64mb | 0.948 | – |
| distinct_half_4mb | 1.000 | 0.980 |
| distinct_half_64mb | 1.141 | – |
| distinct_generic_64mb | plan differs | – |

The candidate's count over the base's; the controls stay within 0.2 %
by instructions and move 2 to 4 % by cycles. distinct_half at 64 MB
writes its pairs to disk now, where `main` held 143 489 kB in a
`hash_mem` of 131 072; distinct_generic is the core's plan in `main`.

The first run found 5.9 % and 8.6 % more instructions for one and two
sets, past the plan's 1 %. Each try was a hypothesis counted on a build
of its own, `ab/1-found.md` to `ab/5-local-pointer.md`:

1. The key of a group's reference carried a mask of the rows ready,
   which reads it on the table's checked path. Copied for every row
   without a mask: 2.5 % and 3.6 %.
2. A build without the step of each set's machine after every batch
   came within 0.7 % and 1.0 %: the step cost a set some 700
   instructions a batch, through the C boundary twice.
3. The machine is asked again only when the memory it weighs moved:
   1.1 % and 1.5 %.
4. PostgreSQL builds with `-fno-strict-aliasing`, so the copy of the
   references read the set's pointer again at every row; through a
   local it vectorizes: 0.5 % and 0.8 %, and the grouping by two keys
   4.9 % fewer than `main`.

The cycles: `ab/7-cycles-overlapped.md` ran while another session's
counted ClickBench loaded the machine, its control 0.90 to 0.99, and is
set aside; `ab/8-cycles-idle.md`, one round a side on an idle machine,
had a control of 0.92 to 1.09, the cores of this machine unequal; three
rounds, `ab/9-cycles-3-rounds.md`, brought it within 4 %, no case
flagged. The instructions are the measure that resolves 1 % here, and
the cycles agree with them within what they resolve.

## 3. ClickBench Q9 and Q10

`cargo clickbench check` and `run --counters --tries 5`, one query and
one mode a run (`clickbench/runs.sh`, its runs in `runs.txt`), at
`work_mem` 64 MB, over the ten files of 1 M rows and over
`hits.parquet`, 100 M rows. Every answer was the core's
(`clickbench/check.txt`). The medians of five readings, in billions of
cycles (`clickbench/counts.txt` has the instructions):

| files | query | workers | on | off | pg_duckdb |
|---|---|--:|--:|--:|--:|
| ten | Q9 | 0 | 1.30 | 10.6 | 0.63 |
| ten | Q9 | 8 | 1.24 | 14.2 | 0.74 |
| ten | Q10 | 0 | 2.11 | 11.0 | 1.01 |
| ten | Q10 | 8 | 2.12 | 15.3 | 1.20 |
| hits | Q9 | 0 | 18.1 | 106 | 7.43 |
| hits | Q9 | 8 | 18.1 | 152 | 7.95 |
| hits | Q10 | 0 | 26.9 | 122 | 11.0 |
| hits | Q10 | 8 | 26.9 | 158 | 11.7 |

- Tessera on takes fewer cycles than off in every row: 0.09 to 0.22 of
  them, Q10 over the ten files at 8 workers among them, which went to
  the core before.
- Over `hits.parquet` Q9 took 83.7 and Q10 97.3 billion cycles in
  stage 2 of ClickBench on `parquet-fdw`, the core's sort over
  Tessera's source; now 18.1 and 26.9, 0.22 and 0.28 of them, within
  the half the plan expected. pg_duckdb with one thread takes 2.4
  times fewer.
- TessAgg runs serially at 8 workers, as a grouping with DISTINCT has
  no partial mode; the counts sum the core's workers.
- Over `hits.parquet` Q9's node holds 123 649 kB of a `hash_mem` of
  128 MB and writes 79 468 659 pairs, 1.16 GB, into 32 partitions
  (`clickbench/analyze-hits.txt`).

## Files

- `spill/final-*`: the summaries and plans of section 1; `first-*`
  the runs before the fixes.
- `ab/`: the reports of `cargo ab`, in the order they ran.
- `clickbench/`: the script, its runs, the checks, the counts, the
  counters of Q9 and Q10's node, and the build of one run.
