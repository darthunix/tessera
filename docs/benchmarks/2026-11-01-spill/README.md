# Spill runs of 2026-11-01

The counters of the spill's kernels and the times of the reads of
spilled text that the change `spill-crate` cites (tasks 2.6, 2.9 and
2.12), and the base later changes of the spill compare with.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power. Other sessions
  of work shared it: each run started with no build or test running,
  and the controls of the times show how far the machine moved.
- **PostgreSQL** master 0497318e442 (20devel), built without
  assertions; `shared_buffers` 2 GB; the bench cluster at scale 1.

## The counters

[`counters.txt`](counters.txt) is the report of `tessera-bench --base
<commit> --bench spill`, the program `spill` of the change's commit
against the same kernels, three processes a side, as root (run
`compare-k4DNGk`). Its counts per call are the first of the program:
a chunk of 1000 records of a join's int4 record packed in 67487
instructions and unpacked in 53489; a chunk of columns of 4 words of
1000 rows packed in 20553, of 70 words of 500 rows in 154238; 1024
rows appended by partition to 8 chunks in 255295 instructions for 4
words and 2117536 for 70; a header written in 69, read in 84. The
packing of records reads up to 0.5 % more or fewer instructions from
one block to the next: it allocates three vectors a call.

## The times

Each report is of `cargo ab measure --base 081173b1 --family exec`:
base and candidate alternating, two rounds a side, a ratio the
candidate's time over the base's, and a control, the same ratio
without Tessera.

- [`ab-final.md`](ab-final.md), run `ab-8ROxGL`, Tessera 016d3108 (the
  tree of the commit of task 2.12, its message written since), 51
  repetitions: `rows_generic` 0.913 by the least time and 0.930 by the
  median, faster; `rows_text` 1.005 and 0.983, and `runs_text` 1.014
  and 1.010, within the control, which moved from 0.903 to 1.095. Its
  times of `runs_text`, `rows_text` and `rows_generic` are the base of
  later comparisons of the spill's reads.
- [`ab-low-bits.md`](ab-low-bits.md), run `ab-BxPkiD`: a rule that took
  every spilled row's partition from the low bits of an unmixed hash,
  1.083 and 1.060 for `rows_text`, whose key's dictionary of values
  takes the low bits of the same hash for its buckets.
