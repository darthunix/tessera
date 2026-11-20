# Row counts checked on whole words of the aggregates, 2026-11-20

The runs of the check that `count`, `sum`, `min` and `max` fail on a
column and a selection of different row counts when they take words
whole, as their row path does. The check cost instructions in its
first place, and was moved; these runs measured both places, and the
second again with every function aligned.

- **Machines**: an Intel Mac, Core i5-1038NG7, the runs typed as root;
  Apple M5 Pro, the runs relayed. Both idle and on mains power.
- **Tool**: `tessera-bench --base 106740d6 --candidate <commit>
  --bench aggregate_int32` and `aggregate_int64`, `--filter
  /words/1024/all/ --filter /words/1024/half/ --filter /words/65/`,
  three processes a side, ten blocks a process. The runs with aligned
  functions took `RUSTFLAGS="-C llvm-args=-align-all-functions=6"` for
  both sides, the Intel Mac only `aggregate_int32` over Datum and the
  65 rows, the M5 Pro both programs without the 65 rows. A report does
  not name the flag.
- **Files**: each run's `report.txt` without its lines `REFERENCE`,
  `PER ROW` and `MODES`, in [`intel/`](intel) and [`m5/`](m5); the
  reports of `--disasm` in [`disasm/`](disasm).
- **Commits**: `106740d6` is on `main`. The measured commits are kept
  on the branch `kernels-row-counts-measured`: `7fc26605` checked the
  row counts at the head of the loops of whole words, and `7631e39d`
  moved the check into the decision. The pull request holds both as
  one commit, rebased onto `main`; `main` had changed none of the
  files these programs build from the kernels and the readers. The
  reports of `--disasm` with `WORKTREE@7fc26605` are the tree of
  `7631e39d` before its commit.

## The check at the head of the loops

`7fc26605`. On the Intel Mac `count` over Datum took 49 more
instructions a call of 1024 rows, +6.4 %, three a word: the check took
a register from the loop of `count_bulk`, which built a constant again
on every word ([`compare-ECeWyC`](intel/compare-ECeWyC.txt),
[`compare-Yyqe2D`](intel/compare-Yyqe2D.txt)). On the M5 Pro the dense
`min` and `max` took 19 more, +1.2 to +1.3 %
([`compare-Phmp5g`](m5/compare-Phmp5g.txt),
[`compare-JTrWzM`](m5/compare-JTrWzM.txt)). The listings of one unit
of code generation showed the check at the head and nothing in the
loop ([`disasm-Y7h8cc`](disasm/disasm-Y7h8cc.txt),
[`disasm-6Ts3cB`](disasm/disasm-6Ts3cB.txt),
[`disasm-1CDXiq`](disasm/disasm-1CDXiq.txt),
[`disasm-0wgY0q`](disasm/disasm-0wgY0q.txt)); the release build, with
its many units, did. Checked after the loop instead, the count kept its
register through the loop and cost the same three instructions, in the
listing of the release build; it was not counted.

## The check in the decision

`7631e39d`. `count_bulk` and `aggregate_bulk` are again those of the
base on both targets; the callers changed
([`disasm-4dQ41C`](disasm/disasm-4dQ41C.txt),
[`disasm-UJGKtZ`](disasm/disasm-UJGKtZ.txt),
[`disasm-I03a3n`](disasm/disasm-I03a3n.txt),
[`disasm-e8XJQU`](disasm/disasm-e8XJQU.txt)).

- Intel Mac, two rounds ([`compare-5IcvBb`](intel/compare-5IcvBb.txt),
  [`compare-tNhIvt`](intel/compare-tNhIvt.txt),
  [`compare-9cmkNK`](intel/compare-9cmkNK.txt),
  [`compare-ZWZpZq`](intel/compare-ZWZpZq.txt)): at 1024 rows 2 to 8
  more instructions a call, 0.42 % at most. At 65 rows the same count
  fails the bar of 1 %: `count` +2, +1.03 %, `min` and `max` +4, +1.05
  to +1.15 %, `sum` +6 to +8, +1.65 to +2.14 %; the cycles +0 to +3 %.
  The `sum` of int4 is no longer inlined into the program's call, and
  pays its call. `count` over Datum in `aggregate_int32` took 10 % more
  cycles at 1024 rows, 155 to 171, in both rounds; in `aggregate_int64`
  it took none.
- M5 Pro, two rounds ([`compare-1BZ3Vd`](m5/compare-1BZ3Vd.txt),
  [`compare-UiesXe`](m5/compare-UiesXe.txt),
  [`compare-uKfetQ`](m5/compare-uKfetQ.txt),
  [`compare-kvozqW`](m5/compare-kvozqW.txt)): instructions -0.55 to
  +0.26 %, no failure of them. `min` of int4 over Datum with rows not
  prepared, which reads its rows one by one, took 19 and 18 % more
  cycles at the same instructions; the first round failed on it, the
  second passed, its median 2 % higher. `count` over dense values took
  5 to 11 % more cycles, about 8 on calls of 70 to 90, under the bar of
  cycles, which counts from 500.

## The same with every function aligned

A function moved against its line of 64 bytes changes its cycles with
the same code. With both sides aligned, `count` over Datum took 169
cycles on both sides of the Intel Mac
([`compare-QOQ6Uj`](intel/compare-QOQ6Uj.txt),
[`compare-Gt8c7u`](intel/compare-Gt8c7u.txt)): its 10 % was the place
of `count_datum_lanes`, called on every word, which the check had moved
from the start of its line to its middle. Every case of 1024 prepared
rows stayed within 2 % there, those with rows not prepared within 6 %;
the 65 rows failed on instructions as before. On the
M5 Pro the `min` with rows not prepared took 1 % fewer cycles with
aligned functions, and the dense `max` of int8 with such rows 6 to 8 %
more. The dense `count` took its 8 cycles more with aligned functions
too ([`compare-rZvXga`](m5/compare-rZvXga.txt),
[`compare-c4cOa6`](m5/compare-c4cOa6.txt),
[`compare-Nk7yZH`](m5/compare-Nk7yZH.txt),
[`compare-tntvNy`](m5/compare-tntvNy.txt)): there the compiler joined
the check with the test of 64 rows in one `ccmp`, an instruction less,
and laid the blocks of the decision out anew.
