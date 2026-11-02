## Context

A batch function computes a PostgreSQL function over a batch of up to
64 rows at a time. Its C side, in `kernels/`, checks the call and calls
an entry point of `tessera-capi`, which reads the C structs and runs a
kernel of `tessera-kernels`, written in safe Rust. A kernel returns a
status; C raises it as the core would. See proposal.md for why three
groups of rules must leave C.

Two words recur below. A **word** is 64 bits of a row mask, so it
stands for 64 rows of a batch. A **lane** is one stored value of a row,
as a vector instruction sees it.

A few facts the design rests on:

- A smallint lives in an int32 lane, its value sign-extended. No
  operation of two smallints overflows an int32: the largest product
  is 2^30. So the int4 kernel computes it. A smallint result fails in
  two ways only: it falls outside -32768 to 32767, or it divides by
  zero.
- A date is an int32, the days since 2000-01-01; a timestamp is an
  int64, the microseconds since 2000-01-01 00:00. The least and the
  greatest value of each are its infinities. A date can reach the year
  5874897, a timestamp only the year 294276.
- A timestamp with time zone (`timestamptz`) is an instant, an int64 in
  UTC. Its local time in the session's zone is the instant plus the
  zone's offset at that second. The zone's offset changes at the zone's
  transitions, such as the start and the end of summer time. Only
  PostgreSQL's time zone library knows them:
  - `pg_next_dst_boundary` gives the offset at an instant and the next
    transition; when it fails, `pg_localtime` gives the offset of that
    one second;
  - `DetermineTimeZoneOffset` gives the offset of a local time, which
    is how a local midnight becomes an instant. It resolves a midnight
    that falls in a gap of summer time as the core does.
- The SQL suite `types` already compares these functions with the core
  in six zones, over 3000 rows spread across 164 years, with half-hour
  offsets, a shift at midnight, local mean time before 1900 and
  infinities. Its output must not change.

## Goals / Non-Goals

**Goals:**

- Each rule is written once, in Rust, where a test, a mutant or a proof
  reaches it. C keeps only what needs PostgreSQL.
- The same answers as today, byte for byte, but for the smallint error
  the proposal shows, which becomes the core's.
- No query gets slower beyond what the A/B tool flags: the int4
  arithmetic keeps its machine code, and the cases of the measurement
  plan are not flagged.
- Rust never calls PostgreSQL. A rule that needs an answer from
  PostgreSQL asks for it by its result, and C answers.

**Non-Goals:**

- The order of errors across an expression. Decision 1 makes one
  operation report the first failing row of its operands. In
  `sum(a / (b + c))` the batch computes `b + c` for all rows first, so
  an overflow there at row 5 still comes before a division by zero at
  row 2, where the core meets the division first. That is the batch's
  order of evaluation, not a rule of a kernel.
- Functions that are not kernels today: a date against a `timestamptz`,
  `date_trunc` with a zone argument, `AT TIME ZONE`.
- The parsing of unit names, which is PostgreSQL's `DecodeUnits`.
- `timestamptz` in the random queries of `tessera-crosscheck`: that is
  the roadmap entry `crosscheck-draws`.

## Decisions

### 1. The smallint range is checked by the arithmetic that computes it

- **Was:** `kernels/int.c` runs an int4 entry point, then walks its
  output in C and fails with 22003 if a value is out of range. An
  error of the int4 kernel at a later row wins over a range error at an
  earlier one.
- **Will be:** the driver of the integer arithmetic,
  `tessera_kernels::int`, takes the range of its result as a type
  parameter. For int4 and int8 the range is the type's own, and the
  check compiles to nothing, so their code does not change. For a
  smallint result the check sits wherever the driver computes:
  - **Row by row.** Each row's operation and its range check are one
    step, so the first failing row gives its error. This holds in the
    driver's row loops and inside a whole word wherever the word is
    computed lane by lane: a division by a column, a division by a
    scalar 0 or ±1, and every division with a scalar on the left.
  - **A whole word of `+`, `-` or `*` by vector code.** The vector
    functions of the module `simd`, which today flag an overflow of the
    word, get smallint forms that flag a result outside the range,
    computed on the vector registers. No lane is read back from the
    output, so no unwritten lane is read. Such a word fails in no other
    way, so its range error is still the first error of the word, and
    the next word has not run.
  - **A division by a prepared divisor** (magnitude two or more)
    cannot leave the range and needs no check.

  A new error, "smallint out of range" (22003), joins
  `ArithmeticError`, after the others, so that their values in the code
  stay as they are. `tessera-capi` gets three entry points: the smallint
  arithmetic with a scalar on the right, on the left, and of two
  columns. `kernels/int.c` calls them; `-x` is `0 - x`, as for int4, and
  the cast `int2(int4)` is `x + 0`, as C computes it today: it needs no
  kernel of its own.
- **Refused:**
  - A second pass in Rust after the int4 kernel. It moves the code but
    keeps the wrong order of errors.
  - A check of the output after a vector word in the driver. It must
    read back the word's output, which only the module `simd` may do
    with `unsafe`.
  - A lane type of its own for smallint. It would copy the int4 vector
    code for one check.

### 2. The bound of a date against a timestamp is a calendar rule

- **Was:** `date_timestamp_evaluate` in `kernels/int.c` computes the
  bound in C and calls the int4 filter. The day of a timestamp, rounded
  down, is computed there, again in `timestamp_day` of
  `kernels/date.c`, and in the calendar's `split` in Rust.
- **Will be:** `tessera_kernels::calendar` gets the rule. For `d op T`
  it gives an operator and a bound `B` in days such that `d op' B` is
  the same test:
  - `d < T` is `d < ceil(T / day)`, `d >= T` is `d >= ceil(T / day)`;
  - `d <= T` is `d <= floor(T / day)`, `d > T` is `d > floor(T / day)`;
  - `d = T` is `d = T / day` when `T` is a midnight; otherwise no date
    is equal, and `d <> T` holds for every date;
  - an infinite `T` is the infinite date of its sign, with the same
    operator.

  A finite date past the last timestamp stays above every finite bound
  and below infinity, which is the core's order. The day comes from
  `split`, the one rule of the day of a timestamp. One entry point,
  `tess_date_timestamp_filter`, computes the bound and runs the int4
  filter over the selected non-NULL rows; C only checks the call.

  The check has two parts, because Kani cannot reason about the
  division by the length of a day in time:
  - **The rule, proven by Kani.** For every valid date, every day and
    time of a valid timestamp (the time at least 0 and less than a
    day), its two infinities, and each of the six operators, `d op' B`
    equals the core's comparison. The core's comparison is PostgreSQL's
    `date_cmp_timestamp`: an infinity first; a date past the last
    timestamp above every finite one; otherwise the date's
    microseconds against the timestamp. The proof states the last step
    over the day and the time: the date against the day, then 0
    against the time, which is the same order while the time is less
    than a day. The valid values are the finite dates from -2451545 to
    2145031948 and the finite timestamps from -211813488000000000 to
    9223371331199999999. A trial of this proof finished in a tenth of
    a second.
  - **The day of a timestamp, by tests.** `split` is Rust's division,
    which truncates toward zero, and a step that rounds a negative
    result down. Its tests check that the day times a day plus the
    time is the timestamp, and that the time is at least 0 and less
    than a day, at
    every edge: a midnight, a microsecond before and after it, both
    signs, and the first and last timestamp; and over random
    timestamps. A trial proof of `split` by Kani did not finish in ten
    minutes, as a division or as the step over any quotient.

  A test also checks the core's comparison written over the day and the
  time against the comparison of the microseconds, at the same edges.
- **Refused:** a test over sampled values only. The rule has a handful
  of edges where one microsecond or one day decides; a proof covers
  them all.

### 3. Local time is a Rust module, and C answers its questions

- **Was:** `kernels/date.c` keeps, in statics of the process, 32 spans
  of the zone's offset and 1024 local midnights. It computes an
  instant's local time, a local day's midnight, how `date_trunc` turns
  a truncated local time back into an instant, and the casts between
  `date` and `timestamptz`, a row at a time.
- **Will be:** the module `tessera_kernels::calendar::zone` holds these
  rules. A call takes some rows of a batch and sorts each selected
  non-NULL row into exactly one of three outcomes, each a mask:
  - **ready**: its value is written;
  - **unknown**: the cache cannot answer it; it asks a question;
  - **for the core**: the core's function must compute it, as today
    for a result out of range, a day before the Julian day 0, a
    midnight the zone cannot give, or an infinity the operation does
    not keep.

  A call writes values only at its ready rows. It changes nothing else:
  the values of other rows, and the inputs a later call needs (the
  instants, the local times, the truncated days), stay as they are.
  The call adds its ready and core rows to the round's masks, and with
  the unknown rows it gives a list of questions: their distinct keys, in
  order, the spans first. A key is an instant in whole seconds for a
  span, or a Julian day for a midnight. C asks PostgreSQL the questions,
  adds the answers to those it has, and calls again over the unknown
  rows with every answer of the batch so far; the call keeps the
  answers in the cache and settles each row from them:

  ```
     C (kernels/date.c)                Rust (calendar::zone)
     ------------------                ---------------------
     call over the rows          --->  answer from the cache:
                                 <---  ready, unknown, for the core,
                                       and the questions
     none unknown: done
     ask PostgreSQL the questions
     call over the unknown rows  --->  keep the answers; settle each
       with every answer so far        row from them
                                 <---  ready, unknown, for the core
  ```

  The answers, with their units and signs:
  - **a span**: from `pg_next_dst_boundary` at the instant, the offset
    in seconds east of UTC (local time is the instant plus it) and the
    next transition, or none. The offset holds from the instant up to
    that transition, so C skips the later questions that fall before
    it and asks once a span. The skip saves calls only: an
    instant no answer covers stays unknown, and Rust asks for it again;
  - **one second**: when `pg_next_dst_boundary` fails, the offset
    `pg_localtime` gives for that second. Rust keeps it as a span of
    that one second, which is exact and covers no other instant.
    PostgreSQL's sources call this failure unreachable today;
  - **a midnight**: for a Julian day, the offset in seconds west of UTC
    that `DetermineTimeZoneOffset` gives. Rust decides which days may
    be asked (from the Julian day 0, a valid date) and computes the
    instant, the local midnight plus the offset, checked for overflow
    and for the range of timestamps. A midnight out of range is kept
    as "no midnight", and its rows go to the core.

  A call settles every row an answer covers, whatever the cache keeps
  after it: a batch of 64 rows spread over decades can need more spans
  than the cache holds. It never asks a question that an answer it was
  given covers; an answer of a span that ends at its own second is
  refused. An operation asks of one kind, spans or midnights, a question
  a row at most, so the rounds end: one call when the cache knows every
  row, two otherwise. `date_trunc` runs two operations, the local times
  of every row and then the truncation, which asks only midnights, so
  that the midnights are learned in the order of the rows after every
  local time, as the C code learned them. The answers are passed whole
  each time, so that a row whose answer a full cache has evicted still
  finds it.

  A call keeps the answers in the cache in the order of the rows that
  need them, row by row, as today's code learns them, a span
  joining a kept span of the same end and offset as today. So, from
  the same cache, after a batch the cache holds what today's code
  holds, with three exceptions:
  - the two new kinds of entry, a span of one second and "no
    midnight", which today's code does not keep;
  - a batch in which today's code, learning in row order from the same
    cache, evicts an entry that a later row of the batch reads, and so
    learns it again; the new code answered that row from the cache in
    its first call and does not. A full cache whose next victim a
    later row needs is enough: rows of spans S32 and S0, in that
    order, over a cache of S0 to S31 whose next victim is S0. The case
    `tstz_spans` of the measurement does this in every batch;
  - after a change of zone, a call that reads the part of the cache
    but needs no entry of it, as when its rows are all NULL or
    infinite: it resets the part before it, where today's code resets
    a part at its first lookup (decision 4).

  Once the two caches differ, they may stay different, and a later
  batch may hit or miss where today's code would not; its answers are
  the same.

  Within a stage, PostgreSQL is asked once for each span and each day
  the cache did not hold when the stage began. Today's code, from the
  same cache, asks at least that often: a row it cannot answer asks,
  unless an earlier row of the batch asked for the same span or day,
  and it may ask again for a span or a day that a later row evicted, or
  for a span whose earlier instants come later in the batch.

  Once every stage is done, the core's functions run over the rows for
  the core in row order. As today, the kernels' work comes before the
  core's rows, so an error of a kernel wins over an error of the core.

  The calls of the module:
  - the local times of a `timestamptz` column, and of one finite value,
    for `extract`; `extract` of an infinity stays with the core, as
    today;
  - the instants of a `date_trunc` from the instants and their local
    times: the calendar truncates the local time; a unit below a day
    keeps the original instant's offset, and asks nothing; a day or more
    takes the local midnight of its first day. The fast path stays as
    narrow as today: a constant, known unit over a column; anything else
    is the core's;
  - `timestamptz(date)`, a date's local midnight; an infinite date is
    the infinite instant, and a date past the last timestamp goes to
    the core before any question;
  - `date(timestamptz)`, the day of an instant's local time; an infinity
    is the infinite date, and a day before the Julian day 0 goes to the
    core;
  - learn a span, a second or a midnight, and reset.
- **Refused:**
  - A callback from Rust into C for the offset. Rust would call
    PostgreSQL, and an error raised there would jump over Rust's
    frames.
  - Asking about the first unknown row only, then calling again. A
    batch whose rows fall on many days the cache does not hold would
    take a call a day: on the measurement's data, 1826 days over 1024
    slots, some 28 rows of a batch miss.
  - Learning in C every span a batch needs before one Rust call. The
    cache may not hold them all, as above.
  - A separate change for the time zone: the maintainer took it into
    this one on 2026-11-02.

### 4. C keeps the memory of the caches; Rust sees an array of words

- **Will be:** the cache is `TessZoneCache`, a struct of 64-bit words of
  a fixed count, declared in `include/tessera/calendar.h`. C keeps one
  in a static of the process. Its zero bytes are an empty cache: a
  midnight record has a word of state, empty, a midnight, or no
  midnight, so the day 0 is not mistaken for an empty slot.
  `tessera-capi` makes a slice of the words, the one `unsafe` step; the
  kernels read the slice through a view that names the words: a header,
  the spans (start, end, offset), the midnights (state, day, instant).
  The count of words is written twice, in the header and in Rust; the
  layout probe of `kernels_layout.h` checks that the two agree, with
  the sizes of the round, the question and the answer, when the module
  loads, and a test of `tessera-kernels` checks it too.
- The entry points require of the caller: the words initialized and
  aligned, as many as the header says, used by this call alone, not
  overlapping the call's inputs and outputs. No Rust reference to them
  outlives the call, so none is held while C asks PostgreSQL.
- C keeps the zone the spans were learned in and the zone the
  midnights were, pointers of PostgreSQL, as today. When the session's
  zone differs from a part's, C resets that part before a call that
  reads it, the calls over one value included: `date_trunc` below a
  day reads no midnight, so it resets none. The spans' next victim
  stays, as today. Today's code resets a part at its first lookup; a
  call that reads the part but needs no entry of it, its rows all NULL
  or infinite, resets it one call earlier, the third exception of
  decision 3.
- The sizes and the eviction stay: 32 spans replaced in turn, 1024
  midnights by day. One thing is new: a midnight the zone cannot give
  is kept, where today it is asked again each time. A slot may then
  hold either kind and is replaced by the next day that falls on it.
  The answers do not change.
- **Refused:**
  - A cache owned by Rust, in a static or allocated once. Rust would
    keep state of the process, which needs `unsafe` or a lock, and
    would allocate.
  - A `#[repr(C)]` struct of the caches shared by C and Rust. It would
    need `unsafe` too, in the kernels or in `tessera-capi`, and its
    layout would be written in both languages. With a slice of words
    the kernels stay free of `unsafe`, and the layout is Rust's alone.

### 5. What stays in C

- `kernels/int.c`: the table of functions and the checks of a call.
- `kernels/date.c`: the table and the checks of a call; the wrappers
  of the calendar functions that do not read a zone (`date_evaluate`,
  `interval_evaluate`); the helpers of arguments and of scratch
  memory; the parsing of unit names and their mapping to the kernels'
  units and fields; the calls of the core's functions (`call_core`,
  `extract_core`) and the numerics of `extract`; the two loops that
  ask PostgreSQL (`pg_next_dst_boundary`, `pg_localtime`, `j2date`,
  `DetermineTimeZoneOffset`) and teach the cache; the reset on a change
  of zone.
- What goes from `kernels/date.c`: the statics of spans and midnights,
  `zone_offset`, `local_timestamp`, `local_midnight`, `local_column`,
  `timestamp_day`, and the arithmetic of `trunc_zoned` and
  `zone_cast_evaluate`.

## What we do not do

- No Kani proof of the time zone rules. Their caches are arrays of
  hundreds of words, too large for a proof. Property tests over model
  zones check them instead (see "Tests").
- No contract for `tessera-kernels` or `tessera-capi`: that is the
  roadmap entry `contracts-of-the-c-path`. This change keeps its rules
  anyway: no call into PostgreSQL from Rust, no allocation over C's
  memory.

## Risks / Trade-offs

- [Kani cannot finish a 64-bit division by the length of a day] → The
  proof covers the rule over the day and the time, which takes no
  division; `split` and the step from microseconds to days are checked
  by tests at every edge (decision 2).
- [More calls into Rust for a `timestamptz`: a batch that misses the
  cache costs a second call over its unknown rows] → The measurement
  plan has a case that misses on every batch.
- [The zone's rules drift from the core's] → The section of the suite
  `types` on the time zone is the oracle and must not change; property
  tests compare the module with a model; a C test forces the answer of
  one second; the mutants of the diff judge the tests.
- [The words of the cache disagree between C and Rust] → The layout
  probe checks the count when the module loads; C never reads a word.
- [Miri cuts the vector code out, so it does not reach the smallint
  forms] → The tests run the vector paths on both machines of CI,
  x86-64 with AVX2 and arm64 with NEON, against the scalar reference.

## Measurement plan

The int4 arithmetic must keep its machine code, and no case may be
flagged slower (docs/measuring.md).

- `cargo run --release --locked -p tessera-bench -- --base <base>
  --disasm --module kernels`: the loops of int4 and int8 arithmetic the
  same as at the base.
- New cases in the family `win` of `bench/pg`, on the bench cluster at
  scale 1. The family sets `TimeZone` to `Europe/Berlin` in its session
  before the cases of `timestamptz`. Their data is a table of its own,
  `bench_tstz`, so that no other family's table changes.
  - `int2_arith`: `sum(s + 1::int2)`, `sum(s * 2::int2)` and
    `sum(-s)` of the smallint column of `bench_dates`;
  - `int2_div`: `sum(s / 3::int2)` and `sum((v % 30000)::int2)`, a
    division and the cast;
  - `date_ts_filter`: a date column against a timestamp constant past
    midnight;
  - `tstz_trunc`: `date_trunc('day', ...)` and `date_trunc('hour',
    ...)` of a `timestamptz` column of hourly instants over five years
    in a scrambled order, about 11 spans of the zone, so the spans hit
    after the first batches and the midnights, 1826 days over 1024
    slots, miss in some 28 rows of a batch;
  - `tstz_cast`: `date(...)` of it, `timestamptz(date)` of the date
    column of `bench_dates`, and `extract(hour from ...)`;
  - `tstz_spans`: `date(...)` of a `timestamptz` column whose row `g`
    lies in span number `(g * 37) % 80` of the 80 spans of the zone
    from 1980 to 2019, at mid-January or mid-July of its year. Since 37
    and 80 have no common factor, any 64 rows in a row lie in 64
    different spans, more than the 32 the cache keeps, so every batch
    misses, today and after.

  `bench/pg/setup.sql` makes `bench_tstz` with the two columns, two
  million rows.
- `cargo ab measure --base <the commit of the cases> --family win
  --cases '^(int2|date_ts|tstz)' --deadline 300` with `PGOPTIONS='-c
  statement_timeout=30s'`, on an idle machine the person confirms. The
  tool flags a case slower when its least time grows by more than 2 %
  and its median moves more than 0.02 beyond the control's. No case
  may be flagged. The ids of the runs go into the pull request.
- A run must take at most five minutes, and has a timeout
  (docs/measuring.md). The tool has no deadline of its own, and a
  signal to it alone would leave its `psql` and the query running. So
  the tool gets one: `cargo ab measure --deadline 300` runs each step
  of a run in a process group of its own, and at the deadline ends the
  group and fails the run. `bench/pg/run.sh` sets
  `client_connection_check_interval`, so that the server stops a query
  whose `psql` is gone. The statement timeout stays, and stops a case
  that runs away. To fit, task 2.2 times one run with `REPETITIONS=1`,
  the plans of every case of the family included, and the repetitions
  are chosen so that the four rounds take well under five minutes. A
  run the deadline stops is kept and explained, not retried as it
  was.
- `tessera-bench` counts the smallint arithmetic in its program
  `arith_int32`, beside the int4 operations over the same rows and
  masks, the values brought into the smallint range: the instructions a
  row of a smallint operation against those of int4.
  The time zone gets no program: its cost is the lookups of the cache
  and the calls of PostgreSQL on a miss, which the whole queries
  above show.

## Migration Plan

None: no format, setting or plan changes. A revert of the commits
undoes the change.

## Decisions of the maintainer

- 2026-11-01: the entry `int2-and-date-bounds-in-rust` was placed in
  the roadmap from the review of what more can move from C to Rust.
- 2026-11-02: this entry is taken up next, and the rules of the time
  zone in `kernels/date.c` join it.

## Files

- C: `kernels/int.c`, `kernels/date.c`; `include/tessera/kernels.h`,
  `include/tessera/calendar.h`, `include/tessera/kernels_layout.h`.
- Rust: `crates/tessera-kernels/src/int/arith.rs`, the new
  `int/range.rs` and `smallint.rs`, `int32/lane.rs`, `int64/lane.rs`,
  `simd/arith.rs` (the smallint forms of the vector functions),
  `ops.rs`, `error.rs`, `calendar.rs`, the new `calendar/zone.rs`;
  `crates/tessera-capi/src/c/int.rs`, `c/calendar.rs`.
- Tests: `crates/tessera-kernels/tests/smallint.rs` and the tests of the
  new modules; new test targets of `tessera-capi`, each added to the
  Miri command of `.github/workflows/ci.yml`; `test/sql/types.sql`; a
  C test callback for the answer of one second.
- Tools: `tools/tessera-ab/src/measure.rs` (the deadline),
  `bench/pg/run.sh`.
- Benchmarks: `bench/pg/win.sql`, `bench/pg/setup.sql`,
  `bench/pg/README.md`; `crates/tessera-capi/benches/support/`, the
  smallint operations of `arith_int32`.
- Documents: `docs/function.md`; `docs/kernels.md`, its list of entry
  points and its paragraph on the time zone.
- `.cargo/mutants.toml`: the module of the new Kani proofs, built only
  under Kani, as the float proofs are.

## Tests

- **Smallint**: each operation against a reference over i16 with the
  core's checks, over the edges (-32768, -1, 0, 1, 32767) and random
  values, in each shape (a scalar on the right, on the left, two
  columns), with NULLs, sparse and full words, several words, dense
  and Datum storage, and both the vector and the row-by-row paths. The
  errors are checked exactly, the first one, not any of a set: a range
  error before a division by zero and the reverse, `-32768 / b` with
  `b = -1` before `b = 0`, and `x % -1`, which is 0. The cast at its
  edges. The entry points under Miri. In `types`, the table of the
  proposal, run with Tessera on and off, both errors in the expected
  output; it gives 22012 on the base and 22003 after.
- **Date bound**: the Kani proof of the rule; the tests of `split` and
  of the core's comparison over the day and the time that decision 2
  lists; unit tests of the whole bound at a midnight, a microsecond
  before and after it, negative timestamps, the first and last
  timestamp, the dates past it, and infinities. The lines of
  `types` on a date against a timestamp, unchanged, and new ones at
  these edges.
- **Time zone**:
  - Property tests over model zones, each a sorted list of transitions
    with offsets from -15 to +15 hours. A model host answers the
    module's questions from the model; a reference computes each answer
    straight from the model, with no cache. One host answers every
    question, and the results must equal the reference. One host skips
    questions as C does: it must be asked once for each span and day the
    cache lacked, and, from the same cache, never more often than a
    model of today's C cache that learns one row at a time. From the
    same incoming cache, both caches must hold the same entries after
    the batch, but for the two exceptions of decision 3, each tested:
    the entries of one second and of no midnight, and a batch in which
    the model, in row order, evicts an entry a later row reads (the
    full cache of decision 3 among them). Over consecutive batches whose
    caches have come apart, only the results are compared. After each
    round the round's first question must be settled. Cases: instants
    over a wide range and every unit of `date_trunc`, more spans in one
    batch than the cache keeps, midnights that fall on one slot (two
    found, a found and a missing one), a reset, a span with no end, a
    span of one second, a midnight out of range, days before the Julian
    day 0, infinities.
  - The entry points through their raw structs under Miri, the calls
    over one value among them, which SQL folds into constants.
  - In `types`: the section on the time zone unchanged byte for byte;
    new rows at the edges today's C reaches (a date past the last
    timestamp, the least date in a zone west of UTC, a midnight out of
    range), and a change of zone and back in one session.
  - A C test callback at an injection point in the loop that asks
    PostgreSQL forces the failure of `pg_next_dst_boundary`, so the
    answer of one second runs through C and Rust.
- The mutants of the changed Rust lines: none survives unless listed
  with a reason.
