## Why

A batch function is a function of PostgreSQL that Tessera computes over
a batch of up to 64 rows at a time, instead of row by row through the
core. Most of them are written in Rust, in the crate `tessera-kernels`,
and called from C through the crate `tessera-capi`. C should only
register them and talk to PostgreSQL. Three groups of rules that decide
what a query returns are still written in C:

- **The range of a smallint result** (`kernels/int.c`). Tessera keeps a
  smallint in an int32 lane, so `a + b` of two smallints runs the int4
  kernel, which cannot overflow on such values. Then the C function
  `smallint_range` walks the kernel's output a second time and fails
  with 22003 "smallint out of range" if a value is outside -32768 to
  32767. The same holds for `-x`, `/`, `%` and the cast `int2(int4)`.
  Because the check comes after the whole batch, a batch that holds
  both an out-of-range row and a later division by zero reports the
  wrong error. On a table of 64 rows, where row 2 is `-32768 / -1` and
  row 5 divides by zero, `SELECT sum(a / b)` with Tessera fails with
  22012 "division by zero"; the core fails with 22003 "smallint out of
  range" at row 2. This was checked on a cluster of PostgreSQL master.
  The check once read the call before it looked at the kernel's
  status, which UBSan found (90de317c).
- **A date compared with a timestamp** (`kernels/int.c`). For `d < T`,
  where `d` is a date column and `T` a timestamp constant, C turns `T`
  once into a bound in days, and the int4 filter compares the dates
  with it. The rule has edges: a timestamp past midnight, a timestamp
  before 2000 (the division rounds toward zero, the day must round
  down), infinities, and dates later than the last timestamp. A mistake
  there does not fail; it changes the answer. Only SQL suites check it.
- **The local time of a timestamp with time zone** (`kernels/date.c`).
  `date_trunc`, `extract` and the casts between `date` and
  `timestamptz` read an instant in the session's time zone. C keeps
  two caches for this: the zone's offset over the span of instants up
  to its next change, and the instant of a local midnight by day. It
  also decides how a truncated local time becomes an instant again: a
  unit below a day keeps the instant's offset, a day or more starts at
  the local midnight. These rules decide the answers around the
  changes of summer time. About 300 lines of C hold them, and only
  hand-written SQL suites test them; the random queries never draw a
  `timestamptz`.

None of these rules is reached by a Rust test, a mutant (a small
change of the code that a test must catch, which CI makes in Rust only)
or a proof by Kani (a tool that checks a property of Rust code for
every input), because none is in Rust. The roadmap entry
`int2-and-date-bounds-in-rust` names the first two; the maintainer
added the third on 2026-11-02.

## What Changes

- **Smallint.** The int4 arithmetic of `tessera-kernels` learns the
  range of its result. For a smallint result each row is checked as it
  is computed, so one operation reports the first failing row of its
  operands, as the core does. A cast from int4 to smallint joins the
  casts. `tessera-capi` gets the entry points; `kernels/int.c` calls
  them and loses `smallint_range`. The batch above then fails with
  22003 like the core.
- **The date bound.** `tessera_kernels::calendar` gets the rule that
  turns a timestamp and an operator into a bound in days, and one entry
  point that bounds and filters. A Kani proof shows that, for every
  date, every day and time of a timestamp and each of the six
  operators, the comparison with the bound gives the core's answer.
  The step from a timestamp to its day and time, a division Kani
  cannot finish, is checked by tests at every edge of a day. The day
  of a timestamp, rounded down, has one owner: the calendar's `split`.
  The two copies in C go.
- **The time zone.** A new module `tessera_kernels::calendar::zone`
  holds the rules of local time: the cache of the zone's spans, the
  cache of local midnights, an instant's local time, the instant of a
  local day's midnight, and how `date_trunc` and the casts put them
  together. Rust never calls PostgreSQL. When its cache does not know
  some rows, it says so and lists the questions they ask; C asks
  PostgreSQL's time zone library, once a span or a day, and passes the
  answers to a second call, which settles those rows and keeps the
  answers in the cache. C keeps the memory of the caches, as an array of
  words, and the identity of the zone.
- C keeps what needs PostgreSQL: the table of functions and their
  registration, the parsing of unit names, the calls of the time zone
  library (`pg_next_dst_boundary`, `pg_localtime`,
  `DetermineTimeZoneOffset`), and the rows the kernels leave to the
  core's functions.
- An error has the same SQLSTATE and message as today, at the same
  row, except the smallint case above, which now reports the core's
  error: an intended change of what SQL shows. Every other result
  stays the same.
- No **BREAKING** change: settings, plans and EXPLAIN do not change;
  the C API only grows.

## Capabilities

### New Capabilities

None. The kernels have no capability folder yet; their functions are
described in `docs/function.md`, which this change updates.

### Modified Capabilities

None. No requirement of an existing spec changes. The change sets
`skip_specs: true`.

## Impact

- Public headers: `include/tessera/kernels.h` (the smallint entry
  points), `include/tessera/calendar.h` (the date bound and the time
  zone: `TessZoneCache` and its calls).
- Code: `kernels/int.c` and `kernels/date.c` (shrink);
  `crates/tessera-kernels/src/int/`, `int32/`, `simd/`, `ops.rs`,
  `error.rs`, `calendar.rs` and the new `smallint.rs` and
  `calendar/zone.rs`;
  `crates/tessera-capi/src/c/int.rs`, `calendar.rs`, `zone.rs`.
- Tests: Rust tests of each rule against a reference written another
  way; a Kani proof of the date bound; property tests of the time zone
  over model zones; tests of the entry points under Miri (Rust's
  interpreter that finds undefined behavior), each new test target
  added to the Miri command of CI. The SQL suite `types`
  must give the same output byte for byte, with a few cases added; the
  smallint case above is added with the core's error. A C test
  callback at an injection point forces the one answer of the time
  zone library that no real zone gives.
- Benchmarks: six cases in the family `win` of `bench/pg`: smallint
  arithmetic, a smallint division and cast, a date filtered by a
  timestamp, and three of `timestamptz` in a zone with summer time, one
  of which misses the cache on every batch; `bench/pg/setup.sql` gains
  a table `bench_tstz` of two `timestamptz` columns.
- Tools: `cargo ab measure` gains a deadline, `--deadline <seconds>`,
  which ends a run with its `psql` and its query, as
  `docs/measuring.md` asks of every run.
- CI: the Kani job proves the date bound with the float proofs of
  `tessera-kernels`; the Miri job runs the new test targets of
  `tessera-capi`.
- Settings, formats, plans and EXPLAIN: unchanged.
- Documents: `docs/function.md`, the paragraphs on smallint, on a date
  against a timestamp and on the time zone; `docs/kernels.md`, its
  entry points.
