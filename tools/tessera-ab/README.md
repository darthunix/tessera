# tessera-ab

A tool that compares two revisions of Tessera for a change that should not
move anything: a refactoring, a file cut in pieces, a helper shared by
several nodes. It does two things:

- **`measure`** times [bench/pg](../../bench/pg/README.md) families with
  the base and the candidate installed in turn, and compares every case
  with the control without Tessera;
- **`moved`** checks that a cut only moved lines between files.

The machine code of the same pair is the third check, in another tool:
`tessera-bench --disasm --module nodes` compares the functions of a C
module and shows where their loops lie
([benches/README.md](../../crates/tessera-capi/benches/README.md#the-c-modules)).
A change to the Rust kernels is measured on processor counters by
`tessera-bench` itself.

## Why it exists

The A/B of a development change was a set of scripts written anew in each
session: install a revision's release build, run a family, install the
other, run it again, compare. Each time they went wrong in a new way. A
series ran twelve empty measurements on a cluster that did not exist. A
parser of object files lost the functions that objdump labels `ltmp0`. And
reading the numbers took judgement every time: is 4 % the change or the
machine? This tool keeps the method in one place:

- **Alternation.** Base and candidate run in turn, `--rounds` times each,
  so that drift of the machine falls on both.
- **The control.** Every case of the families is timed with Tessera on and
  off. The core's time does not depend on the change, so its move between
  the sides is the machine's; a case is flagged only beyond it.
- **Two measures.** The least time of any run is the time in the best
  state of the machine; the mean of the runs' medians is the usual time. A
  case is flagged only when both moved.
- **The right build.** Each run lists the SHA-256 of the installed
  libraries in its `source.txt`; the tool checks them against the side's
  build and stops if another build was measured.

## Before a run

- **A release build of PostgreSQL**, without assertions, named by
  `PG_CONFIG`. The tool installs Tessera into it, so it must be a
  development build of your own.
- **The bench cluster**: `bench/pg/run.sh setup` with the same `PG_CONFIG`,
  once. `run.sh stop` deletes it.
- **A quiet machine**, on mains power if it is a laptop; the report notes
  the power source.

## measure

```sh
PG_CONFIG=~/pg/release/bin/pg_config \
cargo ab measure --base main --candidate my-branch --family anyagg --family anykey
```

`--candidate` defaults to `WORKTREE`, the files as they are, committed or
not. For each family the tool runs base, candidate, base, candidate
(`--rounds 2`, the default). Before each run it installs that side's
release build: the first time it builds it from a snapshot of the revision
(a few minutes, the Rust kernels included), later only `make install`.
`--cases` and `--repetitions` pass `CASES` and `REPETITIONS` to `run.sh`,
`--workers` its workers. A family of 10 to 25 cases takes 10 to 50 seconds
a run at the default 31 repetitions.

The report goes to `target/bench-runs/ab-*/report.md` and to the terminal;
this one is the cut of `agg.c` (`--cases '^g_(sum|avg)_int8$'
--repetitions 101 --rounds 3`):

```
## anyagg

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_int8 | 7.164 | 7.277 | 1.029 | 1.016 | 1.005 |  |
| g_sum_int8 | 5.192 | 5.479 | 1.067 | 1.055 | 1.003 | **slower** |
```

- `base` and `candidate` are the mean of the runs' medians with Tessera.
- `min` and `median` are the candidate's over the base's, by the least time
  and by the mean of medians; below 1 is faster.
- `control` is the same ratio of medians without Tessera.
- A case is **slower** when its `min` and its `median` beyond `control`
  both exceed `--threshold` (2 % by default), and faster in the same sense.

A family whose modes are not `on` and `off` (the probe family's worker
counts) gets a row per mode, without a control.

The tool exits 0 when nothing is slower, 1 when a case is, 2 on an error.
The candidate stays installed.

### A flagged case

1. Measure it again, alone and longer, to tell it from noise:

   ```sh
   cargo ab measure --base main --family anyagg --cases '^g_sum_int8$' --repetitions 101 --rounds 3
   ```

2. If it stays, find out whether the change or the code's place did it.
   The same instructions at another address can take another number of
   cycles: a loop whose head lies in the last bytes of a 64-byte line
   starts every pass with an almost empty fetch. Build both sides with
   their functions aligned and measure again:

   ```sh
   cargo ab measure --base main --family anyagg --cases '^g_sum_int8$' --repetitions 101 \
       --copt=-falign-functions=64
   ```

   If the difference goes, it was the place. `tessera-bench --disasm
   --module nodes --function <name>` shows where the function and its loops
   lie on both sides. In the cut of `agg.c` a sum without GROUP BY was
   5 % slower that way, with its loop identical instruction for
   instruction.

3. If it stays with aligned code, it is the change: look at the machine
   code of the functions the case runs.

## moved

```sh
cargo ab moved --base main --candidate my-branch \
    --from nodes/agg.c \
    --to nodes/agg.c --to nodes/agg_group.c --to nodes/agg_generic.c \
    --to nodes/agg_keydict.c --to nodes/agg_node.h
```

Every non-blank line of the base's `--from` files must be in one of the
candidate's `--to` files, as many times as it stood; order and file do not
matter. A function's type line counts together with the line of its name,
as PostgreSQL's style writes a definition, so that each line of the report
is one function's:

```
4044 lines of the base (a definition's type and name one): 4010 moved as they were, 24 without `static`, 10 missing; 103 new lines

Without `static` now:
  nodes/agg.c:1156: static GenericAgg * generic_init(TessAggState *state, Aggref *agg)
  ...
Missing from the candidate:
  nodes/agg.c:419: static TessRowMask distinct_rows(TessAggState *state, AggValue *value, int nrows,
  ...
New in the candidate:
  nodes/agg.c:33: #include "agg_node.h"
  ...
```

- **Without `static`**: functions one file now calls in another, as a cut
  makes them.
- **Missing**: lines of the base found nowhere: forward declarations a new
  header replaced, or a line the cut changed. Read every one.
- **New**: headers, includes, prototypes.

It exits 0 when no line is missing, 1 otherwise. A rename after the cut
belongs in its own commit, so that the cut's commit passes this check
alone; `git diff --color-moved` shows the same from the other side.

## What is on disk

`target/bench-runs/ab-*/`: `report.md`, the snapshots `base/` and
`candidate/` with their builds, and each side's last `install-*.log`. The
runs of the families are `run.sh`'s own `target/bench-runs/pg-*`, each
with a `build.txt` naming its side and revision.

## Tests

`cargo test -p tessera-ab` checks the parsing of `timings.csv`, a side's
summary, the verdicts against the control, and `moved` on small files. A
measurement needs PostgreSQL and is run by hand.
