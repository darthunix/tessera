# Runs of how a table in memory grows, 2026-11-01

The times and the memory of the change `table-growth-one-owner`: the
whole change against the commit that adds its bench cases, and three of
its commits against their parents.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power. Other sessions
  of work shared it: each run started with no build or test running,
  but the controls of the times moved by up to half, so a ratio within
  its control's spread means nothing.
- **PostgreSQL** master 0497318e442 (20devel), a copy of a release
  build without assertions; `shared_buffers` 2 GB; the bench cluster at
  scale 1. The reports name it so in place of the copy's temporary
  path.
- **Tool**: `cargo ab measure`, base and candidate alternating, two
  rounds a side, 11 repetitions unless a report says otherwise, with
  `PGOPTIONS='-c statement_timeout=30s'`. Every side was built before
  the first run, by a run that timed no case, so that no build fell
  between two timed runs.
- **Reading a report**: a ratio is the candidate's time over the base's,
  below 1 faster; `control` is the same ratio without Tessera, which
  moves with the machine. A case is flagged when it moves past 2 % by
  its least time and by its median beyond the control.

## The whole change

Tessera a0953475 against fd35c5af.

- [`whole.md`](whole.md), run `ab-Y5qAex`: the joins `fk_count`, `dup`
  and `left_dup` of the family `join` and the six growth cases of
  `spill`. Flagged: `build_300k`, 1.621 by the least time and 1.639 by
  the median, and `build_420k`, 1.730 and 1.687, which now spill, as
  the design expects; and `plan_time`, 1.030 and 1.067, with its control
  at 1.043. Nothing else was flagged.
- [`plan-time.md`](plan-time.md), run `ab-Gm1hY1`: `plan_time` alone at
  101 repetitions, 1.021 and 1.006, control 1.002: not flagged. The
  change touches no code of the planner.
- [`spill-4mb.md`](spill-4mb.md), run `ab-R9x8u5`,
  [`spill-16mb.md`](spill-16mb.md), run `ab-lxGWSL`, and
  [`spill-64mb.md`](spill-64mb.md), run `ab-ED50yJ`: the older cases of
  `spill` at each `work_mem`. Flagged: `mixed_text_16mb`, 1.356 and
  1.263, which now spills; `outer_text_4mb` and `mixed_text_64mb`
  flagged faster, against controls that moved by 8 and 10 %. A first
  run of the cases at 4 and 16 MB together, `ab-VxxKW8`, stopped in its
  second run: the family times all its cases in one statement, and
  the timeout of 30 s cancelled it in the core's mode of
  `mixed_text_16mb`. It was split in two.
- [`mixed-text-16mb.md`](mixed-text-16mb.md), run `ab-rELcmL`:
  `mixed_text_16mb` alone at 51 repetitions, 1.371 and 1.333, control
  0.903: slower, 63.4 ms to 84.5 ms.

## Each commit against its parent

- [`distinct-chunks.md`](distinct-chunks.md), run `ab-cbNXgP`: the
  chunks of a DISTINCT set past the first take an eighth of
  `hash_mem`. `distinct_128kb` 0.958 and 0.975, faster.
- [`serial-check.md`](serial-check.md), run `ab-AbWwyD`: the serial
  join's check of its index before a batch. `build_420k` 1.364 and
  1.295, slower; `build_300k` 1.012 and 0.983, since at that commit its
  spill kept every partition in memory (see below).
- [`first-index.md`](first-index.md), run `ab-x1S7V4`: a grouping's
  first index capped by its buckets. `group_fk_1536kb` 1.001 and 0.998.

## Memory

Memory Usage, the partitions of the first level and the bytes written
to disk, from the plans of the runs' last statements with Tessera on;
`hash_mem` is twice `work_mem`. The cases of `spill` not listed did not
change.

- `build_300k`, 300 000 keys at 16 MiB: 14409 kB in memory before;
  13323 kB, 4 partitions, 2367 kB on disk after.
- `build_420k`, 420 000 keys at 16 MiB: 17481 kB in memory before, past
  `hash_mem`; 15883 kB, 4 partitions, 5195 kB on disk after. At the
  commit of the serial check alone it was 17675 kB, and `build_300k`
  16907 kB with 4 partitions and nothing on disk: the spill reserved
  the index of its partitions in memory at 8 bytes a row until the next
  commit.
- `mixed_text_16mb`, 500 000 rows with a text column at 32 MiB: 31881 kB
  in memory before; 30731 kB, 4 partitions of which 3 stay in memory,
  5086 kB on disk after.
- `outer_text_4mb`: 8027 kB and 8 partitions before; 8075 kB and 4
  partitions after, as the spill starts earlier and plans for less.
- `group_fk_1536kb`, 3 MiB: 3009 kB, 8 partitions, 61 regrows before;
  2881 kB, 4 partitions, 88 regrows after.
- `distinct_128kb`, 256 kB: 73 kB before, which missed the set's growth;
  245 kB after. At the commit before its chunks shrank the same query
  counted 1205 kB.
- `regrow_128kb`, 256 kB: 2403 kB, 8 partitions, before and after.
