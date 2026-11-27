# Design

## Context

The Parquet node reads a column of a group a window of up to 1 024 rows
at a time, into Arrow arrays the reader holds. A consumer asks a batch
of up to 64 of those rows for a column; the node then asks the reader,
`tess_parquet_values`, for the batch's rows, by a mask over the rows
the window holds, and copies the values it gets into the batch's
arrays. A full window thus serves 16 batches and 16 calls.

`tess_parquet_values` gives the window's array and the mask to
`ReadPlan::read` of `datum-arrow`. For a plain column of integers,
floating-point numbers, days, times or booleans, the read runs a dense
loop, `dense` or `dense_bits`: for each word of the mask, 64 slots, it
converts every slot into the output's place, asked or not, so that the
loop has no branch a value and runs on the vector units; then it masks
its checks by the slots asked. It goes over every word of the column,
including the words that ask no slot. So every call converts every row
the window holds, 1 024 for a full one, and writes 8 KB, where its batch
asked 64. A dictionary and bytes are read for the slots asked alone.

`ReadPlan::read` promises, for such a column, that it may write the
place of a slot not asked, with the slot's value where it converts and
otherwise with a value of no meaning; the place of a slot asked is its
value. The node relies on that alone: it reads the places of the rows
it asked. The wrapper's rows ask every row of a window in one call, and
`ANALYZE` every row of a window it holds.

Each call of `tess_parquet_values` also allocates, zeroes and frees a
mask of the window's `NULL` rows, as many words as the caller's mask,
which it merges into the caller's mask for the rows asked alone.

A profile of ClickBench's Q3 at the end of `parquet-batch-arrays` puts
`ReadPlan::read` at 24.3 per cent of the query on top of the stack and
the frees of the mask at 2.2 by the call graph.

## Goals / Non-Goals

**Goals:**

- A call that asks the rows of one batch converts the words of the mask
  that hold them, one or two, not the whole window.
- No allocation a call.
- The same values, `NULL` bits and failures for every row asked, and
  the same shapes refused; the same promises of `ReadPlan::read`.
- The dense loop's body as it is: no branch added in it.

**Non-Goals:**

- The node's copy of the values it gets into the batch's arrays, and a
  form of integers in their own width that a consumer would read
  without a copy: the count after this change says what they cost.
- The dictionary's loop and the bytes, which already convert the slots
  asked alone.

## Decisions

A call for the batch of rows 128 to 191 of a window, now and after:

```
 the mask, 16 words:  0   0   1   0   0   ...   0
                      │   │   │   │   │         │
 now:   convert       ███ ███ ███ ███ ███  ...  ███   1 024 values
 after: convert               ███                       64 values
        the others: places kept, words of NULL slots 0
```

### The words from the first asked to the last

- **Was:** `dense` and `dense_bits` convert every slot of every word of
  the column.
- **Will be:** each finds, before its loop, the first and the last word
  of the mask that ask a slot, and converts those words and the words
  between them, as now, a word whole at a time. The words before the
  first and after the last keep their places, and their words of the
  mask of `NULL` slots are written 0, as the read promises of a slot not
  asked. A mask that asks no slot converts nothing and clears the mask
  of `NULL` slots.
- **Why:** it is what the promise already allows. A batch's rows lie in
  one word of the mask, or in two where its first rank is not a
  multiple of 64, so the call converts 64 or 128 values where it
  converted the window. A mask that asks every word, as the wrapper's
  and `ANALYZE`'s do, converts what it converts now. The loop over the
  words between stays as it is: the bounds are found once a call, by a
  scan of at most 16 words, and the loop's body gets no branch.
- **Refused: a branch a word, passing over each word that asks no
  slot.** It converts no more than the words asked, but adds to the
  dense loop a branch on the caller's data, which `docs/measuring.md`
  judges on inputs the predictor has not learned and which no benchmark
  of `datum-arrow` gives. The span converts the words between two
  asked words of a sparse mask, at most what it converts now.
- **Refused: the node converts a window at its first ask and copies
  from it after.** The rows of a window not asked may hold a value that
  fails, a day out of PostgreSQL's range say, which no ask may raise;
  the node would need to ask every held row and to tell such a failure
  from one of a row asked.
- **Refused: a form of integers in their own width.** It spares the
  widening, which the profile does not show as the cost, and changes
  the C API, the kernels and the nodes.

### The mask of `NULL` rows kept in the reader's handle

- **Was:** `tess_parquet_values` allocates `vec![0u64; nulls.len()]`
  for the read's mask of `NULL` rows, then merges it into the caller's.
- **Will be:** the reader's handle keeps a vector for it, cleared and
  sized to the caller's mask at each call, so that it allocates at the
  first call of a reader and never after for windows of the same size.
  The read gets a mask of the caller's length, as now, so that a
  caller's mask shorter than the window is refused as now.
- **Why:** the handle is already the call's `&mut`, lives as long as the
  reader, and is freed with it by its memory context's callback.
- **Refused: a mask of 16 words on the stack, a vector beyond.** It
  needs a comparison with the window's size whose mutants, `<` for
  `<=`, give the same results, which no test can tell and the job of
  mutants fails on; and it ties the entry to the module's size of a
  window.

## Measurement plan

On the M5 Pro, `hits.parquet` in the page cache: `cargo clickbench run
--mode <on|off> --counters --tries 5 --workers 0 --queries <n> --files
hits.parquet`, one query a run, PostgreSQL 19 with `pg_pmu`. The base is
the commit the work starts from, the end the commit of 2.2, each in a
worktree of its own, counted in turns: base, end, base, end.

Before the runs:

- the dense loops' machine code at the base and at the end, by `cargo
  rustc --release -p datum-arrow --lib -- --emit asm`: the loop over
  the words keeps its vector instructions and gains no branch in its
  body;
- the agent asks the maintainer for an idle machine and records the
  power source;
- each worktree's release build is installed and the library the server
  loads checked;
- the plans of the base and the end are compared, query by query.

Queries, with Tessera on:

- Q3, `SUM`, `COUNT` and `AVG` over two integer columns;
- Q4, `AVG` of a 64-bit integer column;
- Q7, `MIN` and `MAX` of a date column;
- Q28, the average length of a string by groups, read in the bytes
  form, which no dense loop reads: a guard.

With Tessera off, `--mode off`:

- Q3, whose wrapper's rows ask each window whole through the same call:
  a guard of the whole-window caller, which keeps its conversions and
  loses the allocation a call;
- Q1, `COUNT(*)`, which reads no column: the control, which the change
  does not touch.

After the counts, the times of Q3, Q4 and Q7, `cargo clickbench run
--mode on --workers 0 --queries <n>` without counters, one run a side,
stand for the claim of speed, as `docs/measuring.md` asks of a change
of a loop's branches.

What is read: a run's count is the median of its five readings, with
their spread; a change is the end's count less the base's over the
base's, in each pair, against the base of the same pair. Cycles decide;
instructions and pg_duckdb's cycles at one thread of the runs of
2026-11-24 stand beside them. A side's spread is the difference of its
two turns; a change smaller than the larger spread of the two sides, or
than the control's move, is no change.

Hypotheses, written before the runs:

- Q3: 18 to 27 per cent fewer cycles: 15 sixteenths of `ReadPlan::read`'s
  24.3 per cent, and the mask's 2.2; fewer instructions.
- Q4: 10 to 22 per cent fewer cycles. Not profiled: one column of Q3's
  two, about 0.5 billion cycles of conversion, against Q4's base.
- Q7: 12 to 25 per cent fewer cycles. Not profiled: one column of Q3's
  two against Q7's 2.3 billion.
- Q28, Q3 with Tessera off, and the control: within 2 per cent; Q3 off
  may lose the allocation a call, a fraction of a per cent.

How the change is judged:

- When a side or the control spreads past 2 per cent, that query is
  counted once more, a third pair of turns; the first two are kept, and
  the side's spread is then the range of its three turns. Still past 2,
  the query is unsettled, which the maintainer decides.
- The change is kept when Q3 loses at least 12 per cent of its cycles
  and no query gains more than 2. A result past the threshold and
  outside its hypothesis passes, and is explained in the report.
- A confirmed excess, or the threshold missed, is profiled and searched
  as CONTRIBUTING.md says before the maintainer is asked.
- Profiles of Q3 at the base and at the end, by `sample`, are kept in
  the report beside the counts: their samples of `malloc` and `free`
  under `tess_parquet_values` tell 2.2's share from 2.1's, and they say
  what the node's copy and the window's reading cost after the change.

The report is kept under `docs/benchmarks/`, as `docs/measuring.md`
says.

## Risks / Trade-offs

- [A caller that reads the place of a slot not asked and counts on its
  value] → it would get the place's old value where it got the slot's.
  The read never promised it; the node, the wrapper and `ANALYZE` read
  the places of the slots they asked. The suites compare every result
  with Tessera off, and `make crosscheck` with the heap.
- [The words of `NULL` slots outside the span] → written 0, as for a
  slot not asked now; the tests prefill the mask with ones and reuse it
  between calls.
- [A sparse mask whose first and last words lie far apart] → the words
  between are converted, no more than now; the node's masks lie in one
  or two words, the whole-window callers' in all.
- [A failure in a word outside the span] → no slot there is asked, so
  none fails now either; a failure in the span keeps its slot.

## Decisions of the maintainer

- Cycles decide the counts; instructions help but come second.

## Files

- `crates/datum-arrow/src/read.rs`: `dense` and `dense_bits`.
- `crates/datum-arrow/src/read/tests.rs`: the tests below.
- `crates/tessera-parquet/src/c.rs`: `tess_parquet_values` and the
  reader's handle; `crates/tessera-parquet/src/c/tests.rs`.
- `openspec/specs/datum-arrow/design.md`,
  `openspec/specs/datum-arrow/contract.md` and
  `openspec/specs/parquet-fdw/contract.md`, at the close.

## Tests

- `datum-arrow`, for every dense family, over a column of 150 slots,
  its last word partial, with `NULL` slots in every word, an output and
  a mask of `NULL` slots filled before with other values and with ones:
  - the middle word asked alone: its slots give their values and its
    `NULL` bits, the places of the other words keep the filled values,
    and their words of the mask are 0;
  - then, reusing both buffers, the first word asked: the middle word's
    places stay as the first call made them;
  - single slots asked: slot 0, slot 63, slot 64 and the last slot of
    the partial word;
  - words 0 and 2 asked: word 1 between them may be converted, the
    last word's places are kept;
  - no slot asked: no place written, the mask of `NULL` slots 0.
- `datum-arrow`: a column of days or times read with the first failing
  slot in the last word of the span, after words outside it hold
  values that would fail: the failure names that slot, and the slots
  asked before it give their values.
- `datum-arrow`: the property test of the dense families, its masks
  drawn so that some words ask no slot.
- `tessera-parquet`: `tess_parquet_values` over two windows of
  different sizes on one reader, its mask of `NULL` rows reused; and
  with a caller's mask of `NULL` rows one word shorter than the window,
  refused with `XX000`, the outputs unchanged.
- The suites and `make crosscheck`: the node's asks, the wrapper's rows
  and `ANALYZE` over every type the reader maps.
