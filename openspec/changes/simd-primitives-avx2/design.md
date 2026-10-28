## Context

See proposal.md, "Why", for the motivation. This section gives the
state of the code and the facts of the two vector sets that shape the
design.

The kernels of int4 and int8 are built in three layers:

```
  int/        drivers shared by int4 and int8: whole words or rows,
              the loops over rows, the loops over whole words
                |
                |  hooks of the lane, IntLane (int32/lane.rs,
                |  int64/lane.rs), and of the sets (set.rs)
                v
  simd/       the vector code of one word of 64 rows: 11 modules,
              about 1700 lines, NEON only
                |
   AArch64 -----+----- elsewhere and under Miri: the module is empty,
                       21 functions are stubs that panic, the sets
                       find no hits, and a `cfg!` keeps every word on
                       the row path
```

- A word is 64 rows. A driver decides once a call whether full words
  go to the vector code: the first word with several selected rows
  decides, by `BULK_MIN_ROWS` and by whether the reader exposes the
  word's storage (`ColumnReader::word_block`).
- The 21 stubs: 9 hooks of the int4 lane, 9 of the int8 lane, the sum
  of int4 words (`int32/aggregate.rs`), `count` and `cast`. The sets
  gate the same way without a stub: `SetValue::block_hits` returns no
  hits.
- NEON is part of every AArch64 processor. The code is compiled with
  it, and each entry point asserts that at compile time, so it is a
  safe function with one `unsafe` call inside.
- AVX2 is not part of every x86-64 processor. A function compiled for
  it, `#[target_feature(enable = "avx2")]`, may run only after the
  processor is known to have it. Calling it is `unsafe` from code
  compiled without it.
- Rust, as of 1.98, adds three rules:
  - `#[target_feature]` cannot be combined with `#[inline(always)]`;
  - a trait's method can carry `#[target_feature]` only if the trait
    declares it `unsafe`;
  - a generic function, or a closure, gets the features of the
    function it is inlined into, not of the one that calls it. Code
    that is not inlined runs without AVX2 and passes its vectors
    through memory.
- The two sets differ in their widths and registers:
  - NEON has 32 registers of 128 bits: 4 int4 lanes or 2 int8 lanes,
    so a word is 16 groups of int4.
  - AVX2 has 16 registers of 256 bits: 8 int4 lanes or 4 int8 lanes,
    so a word is 8 groups of int4.
- The sets also differ in what they offer:
  - AVX2 gives the bits of a comparison directly (`movemask`). NEON
    weights the lanes and adds them across.
  - AVX2 has no saturating add of 32 bits, no minimum or maximum of
    64 bits, and its multiply of 32 bits (`vpmulld`) takes about ten
    cycles on Intel.
- `tessera-bench` builds the benchmark programs of
  `crates/tessera-capi/benches` with `cargo bench --no-run` for the
  machine it runs on. `--disasm` builds them with one codegen unit,
  lists them with objdump and compares every Tessera function. It
  compares registers as they are.
- On this Mac, `cargo build --target aarch64-apple-darwin` builds the
  benchmark programs, and Xcode's objdump (Apple LLVM 17) lists their
  NEON code. The code generated for an explicit target does not depend
  on the machine that builds it.
- The mutants of a pull request's lines run on `ubuntu-24.04-arm`,
  because the NEON code is cut out on x86-64 and its mutants would
  survive unseen (`.cargo/mutants.toml`). A missed mutant fails the
  job.

## Goals / Non-Goals

**Goals:**

- Every operation that has NEON code gets AVX2 code. The AVX2 code
  stays where it saves cycles on the Intel Mac, and where it does not,
  the row path stays.
- The vector code is written once, over primitives, without a cycle
  lost on NEON. The machine code of AArch64 shows that the NEON code
  is the same.
- One binary for every x86-64 processor, with AVX2 chosen at run time.
- A test of the vector path never passes quietly on a machine without
  the vector set.

**Non-Goals:**

- No new kernels and no new types: the class of float8 lanes and
  others come later, over the same primitives.
- No change of what a kernel returns, of its errors, or of the C ABI.
- No timing in the AVX2 work: counters and machine code only.

## Decisions of the maintainer

- 2026-09-20:
  - The x86-64 vector set is AVX2, the integer part of the level
    x86-64-v3. AVX-512 comes later, as a third set, and only on a
    machine where it can be measured.
  - The vector path is chosen at run time by
    `is_x86_feature_detected!("avx2")`, with no `-C target-cpu`.
  - At first, the code was to be two parallel implementations.
- 2026-09-25: primitives instead, on the condition that the trait
  costs no performance. The pilot is the int4 filter on NEON. A kernel
  moves only if its machine code is the same, or if its counters show
  no loss in cycles.
- 2026-09-30: the drivers of both widths are already shared through
  `IntLane`. The pilot concerns the vector code only.
- 2026-10-01: four decisions taken by timing on the M5 Pro are
  measured again on Intel. They are listed in "Decisions measured
  again on Intel" below.
- 2026-10-05: Linux on x86-64 is the target platform. AVX2 is taken
  after `counters-on-x86-and-linux`.
- 2026-10-26:
  - The work is taken now, on the Intel Mac, ahead of the queue.
  - The primitives with their pilot, checked on this Mac through
    `--disasm --target`.
  - Of `bench-in-four-cells`, only the run on the Intel Mac.
  - The mutants of the AVX2 code are judged by a job of their own on
    x86-64.
  - The bar of acceptance of an AVX2 operation is set above the
    tool's warning, at the agent's choice: 10 %.
- 2026-10-27, after the run on the Intel Mac:
  - The bar stays 10 %. On Intel a FAIL of cycles counts when a second
    run of its program confirms it.
  - The run's check is three runs: all programs, the program of the
    FAIL again, and `table_large` after its fixture was fixed.
  - Until the first pull request reaches `main`, the counters take
    the commit of task 2.4 as their base, not `main`.
- 2026-10-27, after the counters of the pilot:
  - The drivers keep their decision for whole words inline, in the
    function of their row loops, with the cheaper decision of the
    comparison. The losses of the row paths of x86-64 that this leaves
    are accepted for the pilot and go to the roadmap, to be settled
    once for every driver.
  - The NEON code may change where the counters of the M5 Pro show no
    loss: the M5 Pro is at hand for runs.

## Decisions

### The whole in one picture

```
  int/ drivers ---- once a call, the token of the operation's sets:
     |                Neon::get()   AArch64 only
     |                Simd::get()   the target's set: Neon on AArch64,
     |                              Avx2 on x86-64 if detected
     v
  IntLane and SetValue hooks, each taking the token it needs
     |
     v
  simd/ kernels, written once over `V: Vectors`:
        filter, arith, aggregate, cast, hash, set
     |
     +--> simd/neon.rs   the primitives on NEON
     +--> simd/avx2.rs   the primitives on AVX2
```

### A token for the vector set

Was:

- Each driver tests `cfg!(all(target_arch = "aarch64", not(miri)))`.
- Each entry point of `simd` asserts NEON at compile time.
- Off AArch64, 21 functions end in `unreachable!`, and the sets return
  no hits.

Will be:

- A zero-sized type stands for each vector set, `Neon` and `Avx2`.
  Each is defined on every target, but has values only where its set
  can run:
  - `Neon::get()` gives a value on AArch64, and nothing elsewhere or
    under Miri;
  - `Avx2::get()` gives a value on x86-64 only after
    `is_x86_feature_detected!("avx2")`. The standard library keeps the
    answer, so asking costs a load.
  - Where a type has no values it is an empty `enum`, so code that
    holds one cannot be reached, and the compiler knows it.
- `Simd` names the set of the target: `Neon` on AArch64, `Avx2` on
  x86-64, and an empty type elsewhere and under Miri.
- A hook asks for the token of the sets where its operation is open:
  `Neon` while the operation has no AVX2 code, `Simd` once it has.
  The driver asks for that token once a call, with its decision for
  whole words. On x86-64, `Neon::get()` gives nothing, so an operation
  open only on NEON takes the row path there, with no test at run
  time.
- The stubs become `match token {}`, with no panic left.
- The token's existence proves that the code compiled for the set may
  run. So the entry points stay safe functions, each with one `unsafe`
  call into the code compiled for the set.

The alternatives:

- Checking the processor at every word. It costs a load and a branch a
  word, and keeps a panic path.
- A constant per family that opens it, beside a single token. It
  works, but it is a second gate to keep in step with the first.
- A pointer to a function, chosen once. It costs an indirect call at
  every word and stops inlining.
- The whole driver compiled twice, once for AVX2. The row paths would
  change their code too, and the code would double.

### Primitives

Was: each kernel module calls the NEON intrinsics itself. The helpers
shared by the kernels are in `simd/mod.rs`: the packing of lane masks
into the bits of a word, the bits of the non-NULL rows, the lane masks
of eight bits, the loads of Datums and the widening of masks.

Will be: a trait, `Vectors`, that `Neon` and `Avx2` implement. Its
operations over int4 and int8 lanes:

- loads of a group from a dense block and from a Datum block (the low
  halves for int4), stores, a value in every lane;
- comparisons for equal and for greater, and the others by swapping
  the operands and inverting;
- the bits of a word from the lane masks of its groups;
- the bits of the non-NULL rows from 64 NULL flags;
- the lane masks of a group from bits, and a select by mask;
- add, subtract and multiply with the lanes that overflow, and the
  quotient by a divisor prepared as a magic number, each set's own;
- minimum and maximum, the widening of int4 to int8 and the check of a
  narrowing;
- xor and shifts for the hashes;
- the sums, minimums and maximums across the lanes of a vector.

The trait gives the number of lanes. A word is 16 groups of int4 on
NEON and 8 on AVX2. A primitive can pack more than one group, as the
packing of bits does on NEON by quarters of 16 rows, so that each set
keeps its best sequence.

The shape of the code, within the rules of Rust above:

```
  safe entry point, takes the token
    --> one function per set, compiled for it, never inlined:
        #[target_feature(enable = "avx2")] (or "neon")
      --> the generic kernel, #[inline(always)], or #[inline]
          and compiled for the set itself
        --> the primitives, #[inline(always)], with the intrinsics
            in `unsafe` blocks whose reason is the token
```

The generic kernel and the primitives get AVX2 only by being inlined
into the function compiled for it. The NEON code has the same shape
today, with closures inlined into its functions. Whether a closure of
a generic kernel is inlined on AVX2 too is what the second check of
the pilot shows.

The rules learnt from the NEON code stay:

- no closure that keeps mutable state of a loop;
- state prepared once a call stays out of the loop;
- a loop over rows stays in the function that checks the lengths.

The exact list of primitives is fixed by the pilot and by each kernel
that moves.

The alternatives:

- Two parallel implementations. This is the fallback if the pilot
  fails. Each kernel and each later class of lanes would be written
  twice.
- Primitives that are themselves compiled for the set: `unsafe` trait
  methods with `#[target_feature]`. They cannot be
  `#[inline(always)]`, so they rely on the inliner's judgment at every
  call. They are the first thing to try if the pilot's second check
  fails.
- `std::simd`, which is not stable, or a crate of portable vectors.
  Each would choose the instructions itself, while the present code
  was shaped by its disassembly; a crate also needs the case that
  `docs/measuring.md`, "Dependencies", asks for.

### The pilot

The pilot moves the int4 filter and the comparison of two columns
(`simd/filter.rs`, with the bits of the non-NULL rows) onto the
primitives, for both sets. It holds when:

1. `tessera-bench --base <the commit before> --disasm --target
   aarch64-apple-darwin` reports every Tessera function of the 13
   programs the same, or `(registers only)`.
2. In the x86-64 listing, the AVX2 code of a word has no call and does
   not spill its lanes to the stack.
3. The tests pass with `TESSERA_REQUIRE_SIMD=1` on this Mac, and in CI
   on `ubuntu-24.04` and `macos-15`.
4. On the counters of this Mac, each operation of `filter_int32` and
   `compare_int32` meets the rule of acceptance of the measurement
   plan.

If the first check fails, the work stops and the maintainer chooses:
counters on the M5 Pro for the difference, or the fallback. If the
second fails, the search follows "Stop and ask" of `CONTRIBUTING.md`:
each try is a hypothesis written first, and two tries in a row that
neither remove a call nor rule out a cause, or about an hour, end it
with a question to the maintainer.

The pilot held, with these findings:

1. The machine code of AArch64 stayed the same in every function of
   the 13 programs through the tokens, the move of the filter onto
   `Vectors` and its AVX2 code (`disasm-gXroZF`, `disasm-vw3ZKZ`,
   `disasm-xpp1tS`). It took care: the NEON primitives are `#[inline]`,
   as the intrinsics are, since `#[inline(always)]` inlined them sooner
   and moved loads and spills; and each driver decides as before, a
   `bool` with the token tested in it, since a closure or a let chain
   in the decision changed the code.
2. On x86-64 the functions of the filter and of the comparison
   compiled for AVX2 have no call in the code of a word and keep their
   lanes in registers. Two paths needed the first fix of "Risks":
   AVX2's primitives of more than one intrinsic are functions compiled
   for AVX2, so that a driver asking for the non-NULL rows, and the
   loader of a column side, a closure the inliner left out, call one
   function rather than each intrinsic.
3. The tests pass with `TESSERA_REQUIRE_SIMD=1` on the Intel Mac; a
   comparison broken on purpose made them fail.
4. The counters, against the commit of task 2.4, two runs each:
   - On the Intel Mac every operation of the filter and of the
     comparison gains far past the bar: 79 to 87 % fewer cycles on
     full words of 1024 rows, 62 to 85 % on random ones, 62 to 83 %
     on 64 and 65 rows (`compare-SiywfG`, `compare-l8lFWP`,
     `compare-JT9XJm`, `compare-RtBK51`).
   - The row paths of x86-64 lose where no whole word is taken: the
     filter 38 cycles on one dense row in 128, the comparison 224
     (4.5 %) on a Datum batch partly prepared and 37 (4 %) on Datum
     words of 8 rows, a few cycles elsewhere. Compiled out on x86-64
     before, the decision for whole words now shares the function of
     the row loops, which run short of registers there; a loop over
     single rows rotated ten registers around one load.
   - Moving the decision out of line traded these losses for others,
     the row loop having moved with it: up to 25 cycles on 65 rows
     with 8 selected, and 12.5 % on full dense words of the int8
     filter, a failure of cycles (`compare-DZDYkn` to
     `compare-ij099L`). It was reverted.
   - Its one gain stayed: the comparison decides from `word_block`
     alone instead of finding the non-NULL rows of the first word.
     On the M5 Pro the NEON comparison is then 12 to 20 % cheaper on
     64 and 65 rows and up to 2 % on 1024, the rest within a cycle
     (`compare-XjGSAM`, `compare-4ZdTUH`); the filters' NEON code is
     the same as before.

### One family at a time, in two commits

After the pilot, every family moves in two commits:

- The first moves its NEON code onto the primitives. Its machine code
  on AArch64 stays the same, checked as in the pilot. Its hooks still
  take `Neon`.
- The second adds the AVX2 primitives that the family needs, and its
  hooks take `Simd`. Each operation is accepted on the counters, and
  an operation that does not gain keeps a hook that takes `Neon`, so
  it stays on the row path on x86-64, with the reason in its comment.

The order: the filter of int8; the arithmetic of int4 with its
divisor; that of int8; the aggregates of int4 with `count`; those of
int8; the widening cast; the hash of int4; that of int8; the sets.

What the moves found:

- The arithmetic of int4 kept its NEON code (`disasm-UuGqn0`) in a
  form of its own, each part found by the machine code:
  - Its kernels are `#[inline]` and compiled for the target's set, as
    NEON's helpers of this code were. With `#[inline(always)]` they
    were merged into one body before each was simplified: the stores
    of a word lost what told them apart from the loads and no longer
    paired, and the loop of a masked word no longer unrolled.
  - A store takes a pointer that the kernel offsets by group, as the
    NEON code did. An address made inside the store, after the lanes,
    let the compiler merge the last groups of the two loaders of the
    division into one block, where the widening multiplication became
    four scalar ones.
  - The quotient by a prepared divisor is each set's own primitive,
    with the divisor's lanes prepared by the set. Prepared by generic
    code, they were inlined earlier and moved the division's code.
  - The lane masks of a masked word come eight rows at a time, two
    groups on NEON and one on AVX2: one group at a time, the loop no
    longer unrolled.
- On AVX2 the int4 arithmetic has no call in the code of a word and
  keeps its lanes in registers (`disasm-xtH7X2`). A sum overflows where
  comparing it with an operand disagrees with the other operand's sign,
  since AVX2 has no saturating addition of 32-bit lanes. The high halves
  of the products, for the overflow of a multiplication and for the
  quotient, come from `vpmuldq` on the even lanes and on the odd ones
  moved down by `vpshufd`: moved down by a 64-bit shift, the odd lanes
  of a product with a value in every lane became three `vpmuludq`.

The sets come last, because their bounds are measured again and they
need a benchmark program of their own, which is added first.

### The run-time choice on x86-64

The vector path needs AVX2 only: BMI and FMA are not used. A build
with AVX2 enabled at compile time, such as with `-C target-cpu=native`,
gets a constant answer from `is_x86_feature_detected!`. The choice
then costs nothing.

### `tessera-bench`: another target, and registers

Was: `--disasm` builds for the machine it runs on and compares
registers as they are.

Will be:

- `--target <triple>` passes the triple to cargo, for `--disasm` only.
  It cannot be used with `--module`, which builds C modules by `make`.
  The report names the target.
- `--disasm` marks `(registers only)` a function whose instructions
  match one for one once the registers of one side are renamed to
  those of the other by a mapping that is one-to-one.
  - A register and its views count as one: `x3` and `w3`; `v1`, `q1`,
    `d1` and `s1`, with any arrangement such as `v1.4s`; `rax` and
    `eax`; `ymm1` and `xmm1`.
  - Registers with a fixed meaning are never renamed: the stack and
    frame pointers, the link register, the zero registers, and the
    registers of arguments and results.

A processor renames registers itself, so their numbers cost nothing
on AArch64, where every instruction is four bytes. On x86-64 a renamed
register can change the length of an instruction, and with it where a
loop's head lies (`docs/measuring.md`, up to 5 %), so there the mark
is not evidence of the same cost. This change uses it on AArch64
only. A renaming that uses another callee-saved register adds a save
and a restore, which are other instructions, so the mark does not hide
it.

### The run of `tessera-bench` on the Intel Mac

`tessera-bench --base WORKTREE` runs all 13 programs as root, on mains
power, on an idle machine. Both sides are the same, so a sound tool
reports no FAIL and no UNSTABLE.

The run records:

- the spread of the minimum cycles between the two sides, on
  operations of 500 cycles or more;
- the operations marked MODES;
- the blocks that read zero.

If it reports a FAIL or an UNSTABLE, the work stops and the maintainer
is asked, with the data. The limits are constants of `report.rs`, and
limits that differ by machine are a trade-off that no rule settles.

It did report both, and the maintainer decided on 2026-10-27:

- `compare-Q3YQPC`, all 13 programs, took 17 minutes, past the five
  of `docs/measuring.md`, as a full baseline does. It reported 1203
  PASS, 1 FAIL of cycles, 2 UNSTABLE, 3 warnings of cycles, 552
  operations with MODES, and no block that read zero. Over the 690
  operations of 500 cycles or more, the minimum cycles of the two
  sides moved by 0.10 % at the median, 0.79 % at the 90th percentile
  and 8.7 % at the 99th: by more than 3 % on 22 operations, by more
  than 10 % on 2, and by 19.6 % at most.
- The FAIL, `count_rows` of `table_int32` on `all/nulls-random`,
  came from two costs of the operation, about 7750 and 8550 cycles. A
  process settles at one of them when it starts and keeps it on every
  core it moves to. All three processes of the base settled at the
  lower cost and all three of the candidate at the higher one, a
  chance of 1 in 64 when both costs are as likely. The control run
  `compare-5NAGGP`, the same binaries on `count_rows` alone, passed
  with both costs on both sides, and its processes kept their costs
  as they moved between cores. So neither the path of the binary nor
  the core decides the cost.
- The two UNSTABLE, `bloom_then_probe` of `table_large` on absent and
  on occupied keys, came from the fixture. The Bloom filter passed a
  different number of absent keys in each batch, so the blocks did
  different work. The fixture now lays the batches out alike, in a
  commit of its own, and `compare-eNZB6g` on `table_large` passed,
  its blocks within 0.0003 % in instructions.

Since that commit the benchmarks of this branch differ from those of
`main`, and `tessera-bench` compares only sides with the same
benchmarks. Until the first pull request reaches `main`, the counters
take the commit of task 2.4 as their base. On x86-64 it takes the row
path, as `main` does.

### Tests and CI

- `TESSERA_REQUIRE_SIMD=1` makes a test fail when `Simd::get()` gives
  nothing on the machine. CI sets it in the Rust jobs: `ubuntu-24.04`,
  an x86-64 runner with AVX2, and `macos-15`, on Apple silicon.
- The tests inside the modules of `simd` become generic over the set.
  They run for the set of the machine.
- The tests of `crates/tessera-kernels/tests` compare a reader that
  exposes its storage with one that does not, so on x86-64 they check
  the AVX2 path against the row path for every open operation.
- The SQL suites and the random queries on `ubuntu-24.04` run the AVX2
  path with no change.
- The Miri job builds the kernels without a set, through
  `tessera-capi`, so the empty tokens compile and stay unreachable.
- An x86-64 processor without AVX2 takes the row path, as every x86-64
  processor does today.

### Mutants

The generic kernels and the NEON primitives compile on arm64, and the
job on `ubuntu-24.04-arm` judges their mutants as it judges the NEON
code today. The AVX2 primitives, `simd/avx2.rs`, compile only on
x86-64. The job on arm64 excludes them in `.cargo/mutants.toml`, with
that reason, and a second job of the lines a pull request changes runs
on `ubuntu-24.04`, which has AVX2, for that file alone, from a
configuration of its own, `.cargo/mutants-avx2.toml`. The token of
AVX2 has no values off x86-64, so that job judges it too, and the
jobs on arm64 the token of NEON.

A mutant that turns a vector set off leaves the row paths, which give
the same answers, and only the test of the set catches it: every
mutants job runs with `TESSERA_REQUIRE_SIMD=1`.

The alternative was to exclude the file with no job, which leaves
every mutant of the AVX2 code unjudged. The second job costs a runner
for the pull requests that change the file. The maintainer chose the
job.

### Decisions measured again on Intel

The four decisions that the maintainer listed on 2026-10-01:

- `BULK_MIN_ROWS = 12`, the selected rows of a word from which whole
  words pay;
- the bounds of `SetValue::WORD_KEYS`, 64 int4 and 16 int8 keys, and
  the compares of the sets without branches;
- the counted loop of a full word in `crates/tessera-capi`, kept out
  of line (`try_fold_full_word`);
- the branch-free read of a NULL slot, `read_masked`.

A value or a form that Intel wants otherwise becomes one per
architecture, and the M5 Pro keeps its own. Where Intel agrees, the
comment that gives the reason says that both machines were measured.

## Measurement plan

The rules of `docs/measuring.md` hold:

- before every run, the maintainer is asked whether the machine is
  idle and on mains power;
- a run takes five minutes at most;
- a run as root is a command that the maintainer types.

The tools: `tessera-bench` on the counters of this Mac,
`tessera-bench --disasm`, and `tessera-bench --disasm --target
aarch64-apple-darwin`.

- **The run on the Intel Mac.** `--base WORKTREE`, all programs. It
  passes with no FAIL and no UNSTABLE. A FAIL of cycles is run again
  on its program, and counts when the second run confirms it.
- **A move of NEON code.** `--disasm --target aarch64-apple-darwin
  --base <the commit before>`, over all programs. It passes with exit
  0, or with `(registers only)` alone. Any other difference stops the
  work for the maintainer.
- **The AVX2 code of a family.** `--base main --bench <program>` on
  the family's programs. On the base, x86-64 takes the row path.
  Until the first pull request reaches `main`, the base is the commit
  of task 2.4.
  - The unit of acceptance is an operation of the program over one
    storage, dense or Datum: `add_scalar` over dense columns, say.
  - The hypothesis: on 1024 rows, in the cases where words are taken
    whole (`all`, `half`, `random`), the operation's minimum cycles
    and its median both fall by more than 10 %.
  - Reproducible means that a second run of the same program shows
    the same fall. The two runs together stay within five minutes.
  - The cost in the other cases (`empty`, `one-per128`, `sparse`,
    `eighth`, batches of 63 to 65 rows) stays of the size accepted on
    NEON, a few cycles a call.
  - The report is summed up as the cases with fewer, the same and more
    instructions, with the median change. The FAILs of instructions on
    short batches are read, not counted.
  - An operation that does not gain keeps the row path on x86-64. A
    loss elsewhere larger than a few cycles a call stops the work for
    the maintainer.
  - The bar is the tool's limit of a failure on cycles, 10 %, rather
    than its warning, 3 %. A gain of that size cannot be the spread of
    identical binaries, which reached 3 % on the minimum of long
    operations on the M5 Pro. NEON gained 20 to 89 %, so a kernel that
    gains less than 10 % is not worth its code. If the run on the
    Intel Mac shows a spread near 10 %, the bar is the maintainer's
    question again.
- **The machine code of AVX2.** After each family, `--base HEAD~1
  --disasm` over all programs on x86-64. The loop of every new word
  has no call, and the AVX2 code of the families already open is the
  same, so that a primitive reshaped for one family does not move
  another unseen.
- **`BULK_MIN_ROWS`.** A candidate with 8 against 12, on the `eighth`
  cases (8 selected rows a word) of the filters, the arithmetic and
  the aggregates. On the M5 Pro an int4 word costs 19 cycles dense and
  27 Datum, against 2.4 to 2.5 a selected row on the row path, which
  puts the break-even at 8 to 11 rows. The hypothesis: an AVX2 word
  costs fewer cycles against Intel's row path, so on Intel whole words
  pay at 8 rows. If the candidate gains on `eighth` and loses nowhere,
  x86-64 takes 8.
- **`SetValue::WORD_KEYS`.** A benchmark program of the sets is added
  first, `set_int32`, with int4 and int8 keys around the bounds: 4,
  16, 17, 64 and 65. The hypothesis: AVX2 has half the registers of
  NEON, so 64 int8 lanes fill all 16 of them, and the bound of int8
  falls on Intel. The int4 bound is measured the same way.
- **The counted loop of a full word.** On the M5 Pro a loop that scans
  bits cost 20 to 30 % more on full words, by where it lay, and the
  counted loop inlined into its caller made empty and sparse calls pay
  for its vector constants. The hypothesis: the same holds on Intel.
  Two candidates against the base on `column_reader`: the counted loop
  inlined, expected to cost more on `empty`, `sparse` and the partial
  cases; and the bit scan on full words, expected to cost more on
  `all`. A candidate that gains by more than 3 % in two runs, with no
  loss elsewhere, makes the form per architecture.
- **`read_masked`.** On the M5 Pro a branch or a chosen address there
  alternated between a fast and a slow mode, by the predictor's state.
  The hypothesis: Intel shows the same. A candidate with a branch
  against the base on `column_reader`, in the cases with NULLs
  (`nulls-mixed`, `nulls-random`), expected to show MODES or more
  cycles. The same rule decides.

## What we do not do

- AVX-512. This processor has it, but the roadmap keeps it for later,
  if AVX2 proves not enough (`avx-512`).
- Runs under Rosetta. Real Intel is at hand, and counters under Rosetta
  count Apple's instructions.
- The prefetch hint of `relhash` on x86-64, `_mm_prefetch`. It is
  SSE, which every x86-64 has, not AVX2, and `relhash` has its own
  spec, design and contract, the last in the open change
  `table-crate`. A roadmap entry takes it.
- Linux without `sudo`, and the cell in a report: they stay in
  `bench-in-four-cells`.
- The loops that save their values around the call of a kernel
  (`sturdy-bulk-loops`). This change keeps a call for each word, as
  today.
- New classes of lanes, such as float8, date or timestamp.
- Tuning for AMD processors. The Linux servers of the target platform
  are checked in `linux-x86-support`.

## Risks / Trade-offs

- [The generic kernel, a closure or a primitive is not inlined into
  the function compiled for AVX2, and runs without AVX2, with its
  vectors through memory] → The second check of the pilot. The first
  fix to try is primitives compiled for AVX2 as `unsafe` trait
  methods; then kernels without closures; then the fallback.
- [AVX2 has 16 registers, and the code of a word of int8 or of a set
  with many keys spills] → The x86-64 listing of every family. The
  bounds of the sets are measured again.
- [`vpmulld` is slow on Intel, so the hashes and the products gain
  less] → Acceptance by operation. An operation that loses keeps the
  row path on x86-64.
- [Cycles on Intel spread more than the limits allow] → The run on
  the Intel Mac comes first, and stops the work for the maintainer.
- [Moving the NEON code changes it] → The machine code of AArch64 is
  compared at every move, and the work stops at the first difference.
- [A runner of CI has no AVX2] → `TESSERA_REQUIRE_SIMD=1` turns that
  into a failure.
- [The mutants of the AVX2 code go unjudged] → The job on x86-64.
- [One Intel machine decides for every x86-64] → The AMD Zen servers
  of the target platform are left to `linux-x86-support`.

## Two steps, two pull requests

- Step 1, sections 1 to 3 of the tasks: the tools, the run on the
  Intel Mac and the pilot. Its pull request reports the pilot, and the
  maintainer's merge decides between the primitives and the fallback.
- Step 2, sections 4 to 6: the other families, the decisions measured
  again, and the close.
