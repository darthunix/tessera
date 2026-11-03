# Runs of inputs the predictor cannot learn, 2026-11-03

The runs of the change `bench-unlearned-inputs`. A pilot chose how the
benchmark cases drawn at random get inputs that the branch predictor
has not learned: a ring of 64 inputs, every call taking the next. Then
every program whose cases changed ran against itself on both machines,
to record the spread of the new cases.

- **Machines**: an Intel Mac, Core i5-1038NG7, the runs typed as root;
  Apple M5 Pro, the runs relayed. Both idle and on mains power.
- **Tool**: `tessera-bench --base WORKTREE`, so both sides are built
  from the same worktree; three processes a side, ten blocks a
  process. Each run names its program with `--bench` and its cases
  with `--filter`; the reruns took `/1024/random/nulls-random/` and
  `/1024/all/nulls-random/`, and the runs of the new cases
  `/shuffled/` too.
- **Files**: each run's `report.txt` without its lines `REFERENCE`,
  `PER ROW` and `MODES`, in [`intel/`](intel) and [`m5/`](m5). The line
  of an operation holds its medians and MODES. Between identical
  sides every change is noise.
- **Commits**: a report names the commit its sides were built at. The
  branch was rebased onto `main` after the runs, which changed the ids
  of its commits; a commit keeps its subject. `98b72bb` is "benches:
  the pilot of inputs the predictor cannot learn", `1fb5002` "benches:
  the pilot of the sets on shuffled values", `becb677` "docs: which
  benchmark cases the predictor learns" and `116693a3`
  "bench-unlearned-inputs: the runs of the new cases". `ca81060c` is a
  probe on the branch `probe-compare-ring-footprint`, never merged.

## The pilot

Two programs, each case measured as three forms and their controls:
a ring of `K` inputs, one input `K` times longer, and a fresh input
before every call.

- The first pilot, at `98b72bb`: `column_reader` on the Intel Mac,
  [`compare-5jX1S4`](intel/compare-5jX1S4.txt) and
  [`compare-p7fnPD`](intel/compare-p7fnPD.txt), and on the M5 Pro,
  [`compare-US05n5`](m5/compare-US05n5.txt) and
  [`compare-ef8glM`](m5/compare-ef8glM.txt); `set_int32` on the Intel
  Mac, [`compare-ptHi4q`](intel/compare-ptHi4q.txt) and
  [`compare-kQdvT1`](intel/compare-kQdvT1.txt). The ring passes from
  `K` of 16 on the Intel Mac and from 64 on the M5 Pro, where the fold
  misses 12.2 times a call at 8 and 15.0 at 64. The control of the
  fresh input costs 37 % more cycles than one input on the Intel Mac
  and 24 % on the M5 Pro; the long input's control misses as much as
  the form on the Intel Mac. The case of the sets, values spread
  evenly, could not reach its model.
- The second pilot of the sets, on values in random order, at
  `1fb5002`: [`compare-xtVxg1`](intel/compare-xtVxg1.txt) and
  [`compare-3cQu4h`](intel/compare-3cQu4h.txt). The ring of 64 misses
  1441 and 1438 times a call, where the model says 1351.

## Identical sides over the rings

At `becb677`, every program with new cases, on the Intel Mac and on
the M5 Pro. No FAIL, no UNSTABLE and no warning. For each: the largest
change of the minimum cycles over the operations of 500 cycles or
more, of the median, and the largest MODES, Intel first.

- `column_reader`: [`compare-WbHHrW`](intel/compare-WbHHrW.txt),
  [`compare-IEzeFJ`](m5/compare-IEzeFJ.txt); 1.5 and 0.3 %; 2.0 and
  0.5 %; 1.16 and 1.03.
- `aggregate_int32`: [`compare-WZP0Su`](intel/compare-WZP0Su.txt),
  [`compare-6RSrKR`](m5/compare-6RSrKR.txt); 0.8 and 1.2 %; 4.8 and
  1.0 %; 2.01 and 1.33.
- `aggregate_int64`: [`compare-8hTjRR`](intel/compare-8hTjRR.txt),
  [`compare-86tzI4`](m5/compare-86tzI4.txt); 0.7 and 1.1 %; 6.2 and
  1.6 %; 2.13 and 1.37.
- `arith_int32`: [`compare-O4abX0`](intel/compare-O4abX0.txt),
  [`compare-6xxIT8`](m5/compare-6xxIT8.txt); 2.0 and 2.5 %; 33 and
  3.7 %; 2.04 and 2.05.
- `arith_int64`: [`compare-vZfCEG`](intel/compare-vZfCEG.txt),
  [`compare-PyC46F`](m5/compare-PyC46F.txt); 1.4 and 2.2 %; 7.8 and
  2.2 %; 1.80 and 1.07.
- `cast_int32`: [`compare-v6X2p9`](intel/compare-v6X2p9.txt),
  [`compare-hxENpu`](m5/compare-hxENpu.txt); 1.0 and 0.9 %; 1.5 and
  0.5 %; 1.82 and 1.02.
- `hash_int32`: [`compare-Itc6Cq`](intel/compare-Itc6Cq.txt),
  [`compare-BNAl8e`](m5/compare-BNAl8e.txt); 2.2 and 0.5 %; 5.2 and
  0.6 %; 2.43 and 1.58.
- `hash_int64`: [`compare-aOyBoB`](intel/compare-aOyBoB.txt),
  [`compare-vrsFHw`](m5/compare-vrsFHw.txt); 1.0 and 0.5 %; 7.8 and
  1.1 %; 1.80 and 1.11.
- `set_int32`: [`compare-BAjngB`](intel/compare-BAjngB.txt),
  [`compare-sWuGJ1`](m5/compare-sWuGJ1.txt); 1.8 and 4.4 %; 24 and
  9.5 %; 1.83 and 1.52.
- `table_int32`: [`compare-aGVUYz`](intel/compare-aGVUYz.txt),
  [`compare-y1fGGr`](m5/compare-y1fGGr.txt); 1.9 and 4.1 %; 9.4 and
  12 %; 1.58 and 1.53.
- `filter_int32`: [`compare-p2GjIk`](intel/compare-p2GjIk.txt),
  [`compare-3w1jki`](m5/compare-3w1jki.txt); 0.5 % and no operation
  of 500 cycles; 5.4 and 0.3 %; 1.77 and 1.26.
- `filter_int64`: [`compare-SSAeGd`](intel/compare-SSAeGd.txt),
  [`compare-G9jjzK`](m5/compare-G9jjzK.txt); 0.3 % and no operation
  of 500 cycles; 0.7 and 2.2 %; 1.62 and 1.18.
- `compare_int32`: [`compare-SCAshC`](intel/compare-SCAshC.txt),
  [`compare-GK3FcH`](m5/compare-GK3FcH.txt); 0.0 and 21 %; 1.7 and
  3.7 %; 1.71 and 1.33.

The fold of `column_reader` misses about 17 branches a call over
`random` on the Intel Mac and 14 on the M5 Pro; the search of
`set_int32` over `shuffled`, 65 keys, 1418 to 1489 on the Intel Mac.
Before the rings the same cases missed far less there:
[`before-the-rings.txt`](intel/before-the-rings.txt) holds the lines
of the fold in `compare-F6x7L1`, where a side missed 17 times a call
or almost never, of the search in `compare-jrOJVK`, 12.7 to 17, and of
`probe_hit` of `table_int32` in `compare-zOahIK`, 8.7. Those runs
compared other branches, so only their misses are a guide.

## After the rebase

At `116693a3`, `arith_int32` again, now with its smallint groups on
the ring of their case: 36 operations, all PASS.

- [`compare-2ikC92`](intel/compare-2ikC92.txt), the Intel Mac: the
  minimum moved by 1.5 % at most, the median by 2.5 %.
- [`compare-XKaRLo`](m5/compare-XKaRLo.txt), the M5 Pro: all 18
  Datum operations were cheaper on the second side, by up to 7.2 %,
  and the median moved with them; the dense ones by 1.4 % at most.

## The two levels of the M5 Pro

The 21 % of `compare_int32` on the M5 Pro is one block, the first of a
process, at 985 cycles; the other blocks keep each process at a level
of its own, about 1200 to 1300:
[`compare-GK3FcH-blocks.txt`](m5/compare-GK3FcH-blocks.txt), the
cycles a call of every block. The probe measured the pair drawn at
random as one pair, a ring of 4 pairs inside L1 and a ring of 64, in
one build: [`compare-SXZg8f`](m5/compare-SXZg8f.txt) and its blocks,
[`compare-SXZg8f-blocks.txt`](m5/compare-SXZg8f-blocks.txt). Over
Datum the medians of the six processes lie within 1.2 % for one pair,
10.2 % for the ring of 4 and 11.9 % for the ring of 64, so the
footprint past L1 is not what spreads them. What does is not known.
