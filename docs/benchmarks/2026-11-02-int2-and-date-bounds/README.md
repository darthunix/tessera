# Runs of smallint arithmetic, the date bound and the zone, 2026-11-02

The runs of the change `int2-and-date-bounds-in-rust`: the smallint
arithmetic and the bound of a date against a timestamp moved to Rust,
and an attempt to move the rules of local time of a zone, which the
change left out because of these runs.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power, no build or
  test of another session running at the start of a run.
- **PostgreSQL** master 0497318e442 (20devel), a copy of a release
  build without assertions; `shared_buffers` 2 GB; the bench cluster at
  scale 1, with the table `bench_tstz` of `bench/pg/setup.sql`.
- **Tool**: `cargo ab measure --deadline 300`, base and candidate
  alternating, two rounds a side, 21 repetitions, with
  `PGOPTIONS='-c statement_timeout=30s'`, every side built before the
  first timed run. The base is c14180a5, the commit that adds the six
  cases to the family `win`, kept in the history of the branch
  `zone-rules-in-rust-rounds`.
- **Reading a report**: a ratio is the candidate's time over the base's,
  below 1 faster; `control` is the same ratio without Tessera, which
  moves with the machine. A case is flagged when it moves past 2 % by
  its least time and by its median beyond the control.

## The change

- [`whole.md`](whole.md), run `ab-SvKiRM`, Tessera 9356c3fb, the
  change's code on the `main` of 347571da: `int2_arith` 0.977 by the least time and 1.007 by
  the median, `int2_div` 0.994 and 1.019, the cases of the zone, back
  on the C code, 0.92 to 1.02. `date_ts_filter` was flagged, 1.021 and
  1.102, with its control at 1.033: Spotlight indexed the new builds
  through the run, about four cores of the machine, and the controls
  of the six cases moved from 0.915 to 1.034.
- [`date-ts-filter.md`](date-ts-filter.md), run `ab-8irMlU`, once the
  indexing had stopped: `date_ts_filter` alone at 101 repetitions,
  1.012 and 1.027, control 1.008: not flagged. The first run of the
  attempt with the zone, `ab-Y7Hq4a` below, had it at 0.981 and 0.964.
  An 8 ms query, it moves by a percent or two between runs.
- [`counts.md`](counts.md): the instructions of the smallint arithmetic
  against int4's in the program `arith_int32`, and the machine code of
  the int4 and int8 kernels against `main`.

## The zone in rounds, left out

The attempt kept the rules of the zone in Rust and had C ask PostgreSQL
between calls: a call answered what its cache knew and returned the
rest as questions; C asked the time zone library and called again with
the answers. It is on the branch `zone-rules-in-rust-rounds`.

- [`zone-rounds.md`](zone-rounds.md), run `ab-Y7Hq4a`, Tessera
  a168afe9: `tstz_cast` 1.711 by the least time and 1.679 by the
  median, `tstz_trunc` 1.805 and 1.779, `tstz_spans` 2.399 and 2.472,
  controls within 3 %. `tstz_spans` took 239 ms against 130 ms without
  Tessera. The cases of smallint and of the date bound were not
  flagged.
- [`profile-tstz-spans.txt`](profile-tstz-spans.txt),
  [`profile-tstz-trunc.txt`](profile-tstz-trunc.txt),
  [`profile-tstz-cast.txt`](profile-tstz-cast.txt): where the time
  went. The library itself (`pg_next_dst_boundary`,
  `DetermineTimeZoneOffset`) takes about what the C code spent; the rest
  is the work around each row. A row the cache lacks goes through Rust
  twice, once to ask and once to learn the answer, and through C once,
  where the C code took it once.
- [`zone-rounds-answers-by-halves.md`](zone-rounds-answers-by-halves.md),
  run `ab-21xBNW`: the answers searched by halves in place of in turn,
  with the same answer for every row; 1.659, 1.892 and 2.371 by the
  median, no change. A trace of `xctrace` to the instruction then
  found the time spread over the reading of the day, the cache and the
  search, no single place.

The roadmap's entry `zone-rules-in-rust` takes it from here.
