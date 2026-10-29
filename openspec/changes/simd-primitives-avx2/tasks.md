## 0. How the work goes

Every task is one commit on the branch `simd-primitives-avx2`, in the
worktree of that name. Sections 1 to 3 are the first step and reach
`main` in a first pull request; sections 4 to 6 are the second step,
in a second. Each commit passes `make rust-check` and
`.github/scripts/check-specs.sh`, run with the version of OpenSpec
that `ci.yml` names; the check named in a task comes on top. A commit
of more than about 500 lines of production code says in its message
why it is not split.

The words used below:

- **"The move check"**:
  `cargo run --release --locked -p tessera-bench -- --base HEAD~1
  --disasm --target aarch64-apple-darwin`, over all programs, which
  exits 0 or marks only `(registers only)`.
- **"The x86 listing"**: `cargo run --release --locked -p
  tessera-bench -- --base HEAD~1 --disasm`, over all programs, on the
  Intel Mac. The loop of every new word has no call, and the AVX2
  code of the families already open is the same.
- **"As root"**: a command that the maintainer types on the Intel Mac,
  after `sudo -v`, on an idle machine on mains power. The agent asks
  first.
- **"The counters"**: as root, `cargo run --locked -p tessera-bench --
  --base main --bench <program>`, twice, judged operation by operation
  by the measurement plan of the design, with the runs' directories in
  the pull request. Until the first pull request reaches `main`, the
  base is the commit of task 2.4, whose benchmarks are those of this
  branch.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`. Check:
      `openspec validate simd-primitives-avx2 --strict`;
      `.github/scripts/check-specs.sh`

## 2. `tessera-bench` on the Intel Mac

- [x] 2.1 `--target <triple>` for `--disasm`: both sides built for the
      triple, the target named in the report, and the option refused
      without `--disasm` and with `--module`; a line in the README of
      the benchmarks. Check: `cargo test --locked -p tessera-bench`;
      `cargo run --release --locked -p tessera-bench -- --base HEAD
      --disasm --target aarch64-apple-darwin` exits 0
- [x] 2.2 `(registers only)` in `--disasm`: a one-to-one renaming of
      registers, where a register and its views count as one and the
      registers with a fixed meaning are never renamed; a line in the
      README that the mark is evidence on AArch64 only. Check:
      `cargo test --locked -p tessera-bench`, with listings of arm64
      and of x86-64 in which a consistent renaming is marked, while a
      renaming that maps two registers to one, or one to two, or that
      moves the stack pointer or an argument register, is a change
- [x] 2.3 The link to `pmu` in `crates/tessera-capi/benches/README.md`
      points to `tools/pmu`. Check: the path of the link exists from
      the README's folder
- [x] 2.4 Run `tessera-bench` on the Intel Mac and correct the texts:
      the README of the benchmarks, `docs/measuring.md` and "Accepted
      limits" in `CONTRIBUTING.md` say that `tessera-bench` counts on
      macOS, on Apple silicon and on Intel, and that on Linux, where
      `pmu` already counts, it is planned. A fix of the tool or of a
      benchmark that the run calls for is a commit of its own before
      this one. Check: as root, `cargo run --locked -p tessera-bench
      -- --base WORKTREE` over all programs; a FAIL of cycles is run
      again on its program, and a benchmark fixed is run again; the
      runs end with no FAIL and no UNSTABLE; their spread, MODES and
      zero readings go into the pull request

## 3. The pilot

- [x] 3.1 The tokens of the vector sets: `Neon`, `Avx2` and `Simd`,
      each an empty type where its set cannot run. Every driver asks
      for `Neon` instead of testing `cfg!`, the hooks take it, the 21
      stubs become `match` on it, and the sets take it too; on x86-64
      no operation opens yet. The test that fails under
      `TESSERA_REQUIRE_SIMD=1` when `Simd::get()` gives nothing.
      Check: the move check; `cargo clippy --locked -p tessera-kernels
      --all-targets --target aarch64-apple-darwin` and `--target
      x86_64-unknown-linux-gnu`; `TESSERA_REQUIRE_SIMD=1 cargo test
      --locked -p tessera-kernels` on the Intel Mac; no
      `unreachable!` left in the hooks; the Miri job of CI, which
      builds the kernels without a set
- [x] 3.2 The trait `Vectors` with its NEON implementation, and
      `simd/filter.rs`, the filter of int4 and the comparison of two
      columns, written over it. Check: the move check
- [x] 3.3 The AVX2 implementation of the filter's primitives. The
      filter of int4 and the comparison of two columns take `Simd`;
      the documentation of the crate and of `simd`, and
      `docs/limitations.md`, name AVX2 for what is open. Check:
      `TESSERA_REQUIRE_SIMD=1 cargo test --locked -p tessera-kernels`
      on the Intel Mac; the x86 listing; the counters of
      `filter_int32` and `compare_int32`
- [x] 3.4 `TESSERA_REQUIRE_SIMD=1` in the Rust jobs of CI. Check: CI
      green on `ubuntu-24.04` and `macos-15`, with the id of the run
- [x] 3.5 The mutants of the AVX2 code: `simd/avx2.rs` excluded from
      the job on arm64 with its reason, and a job of the changed lines
      of that file on `ubuntu-24.04`, reading
      `.cargo/mutants-avx2.toml`; every mutants job requires the vector
      set of its runner. Check: on the Intel Mac,
      `TESSERA_REQUIRE_SIMD=1 cargo mutants --config
      .cargo/mutants-avx2.toml` misses no mutant, with cargo-mutants
      installed as `mutants.yml` installs it; the job green on the pull
      request
- [x] 3.6 Open the first pull request, into `main`, with the pilot's
      result. The maintainer's merge decides between the primitives
      and the fallback of the design. Check: every item of its
      description points to a test, a command or the id of a run

## 4. The other families

Each pair moves one family. The first task of a pair moves its NEON
code onto the primitives, and its hooks still take `Neon`. The second
adds its AVX2 primitives, its hooks take `Simd`, and it corrects the
texts that count its lanes. An operation whose AVX2 code does not gain
keeps a hook that takes `Neon`, with the reason in its comment.

- [x] 4.1 The filter of int8, `simd/filter64.rs`, over the primitives.
      Check: the move check
- [x] 4.2 The filter of int8 on AVX2. Check: `TESSERA_REQUIRE_SIMD=1
      cargo test --locked -p tessera-kernels`; the x86 listing; the
      counters of `filter_int64`
- [x] 4.3 The arithmetic of int4 with its divisor, `simd/arith.rs`,
      over the primitives. Check: the move check
- [x] 4.4 The arithmetic of int4 on AVX2, the division by a magic
      number included. Check: as in 4.2, on `arith_int32`
- [x] 4.5 The arithmetic of int8, `simd/arith64.rs`, over the
      primitives. Check: the move check
- [x] 4.6 The arithmetic of int8 on AVX2. Check: as in 4.2, on
      `arith_int64`
- [x] 4.7 The aggregates of int4 and `count`, `simd/aggregate.rs`,
      over the primitives. Check: the move check
- [x] 4.8 The aggregates of int4 and `count` on AVX2. Check: as in
      4.2, on `aggregate_int32`
- [x] 4.9 The aggregates of int8, `simd/aggregate64.rs`, over the
      primitives. Check: the move check
- [x] 4.10 The aggregates of int8 on AVX2. Check: as in 4.2, on
       `aggregate_int64`
- [ ] 4.11 The widening cast, `simd/cast.rs`, over the primitives.
       Check: the move check
- [ ] 4.12 The widening cast on AVX2. Check: as in 4.2, on
       `cast_int32`
- [ ] 4.13 The hash of int4, `simd/hash.rs`, over the primitives.
       Check: the move check
- [ ] 4.14 The hash of int4 on AVX2. Check: as in 4.2, on `hash_int32`
- [ ] 4.15 The hash of int8, `simd/hash64.rs`, over the primitives.
       Check: the move check
- [ ] 4.16 The hash of int8 on AVX2. Check: as in 4.2, on `hash_int64`
- [ ] 4.17 A benchmark program of the sets, `set_int32`: `IN` of int4
       and of int8 keys over the reading cases, with 4, 16, 17, 64
       and 65 keys, known to `tessera-bench` and listed in the README
       of the benchmarks. Check: `cargo test --locked -p tessera-capi
       --benches`; `cargo bench --locked -p tessera-capi --bench
       set_int32 -- --list`
- [ ] 4.18 The sets, `simd/set.rs`, over the primitives. Check: the
       move check
- [ ] 4.19 The sets on AVX2, and the lanes of a vector in
       `docs/kernels.md`, "Sets". Check: as in 4.2, on `set_int32`

## 5. Decisions measured again on Intel

Each task follows the hypothesis written for it in the measurement
plan of the design. It is a commit only when a value, a form or a
reason changes: one per architecture, or the comment that gives the
reason saying that both machines were measured.

- [ ] 5.1 `BULK_MIN_ROWS`: 8 against 12 on the `eighth` cases. Check:
      as root, twice, the candidate in the working tree against
      `HEAD` on `filter_int32`, `arith_int32` and `aggregate_int32`
- [ ] 5.2 `SetValue::WORD_KEYS` and the compares of the sets without
      branches. Check: as root, twice, the candidates against `HEAD`
      on `set_int32`
- [ ] 5.3 The counted loop of a full word, `try_fold_full_word`: the
      loop inlined, and the bit scan on full words. Check: as root,
      twice, each candidate against `HEAD` on `column_reader`
- [ ] 5.4 The branch-free read of a NULL slot, `read_masked`, against
      a branch. Check: as root, twice, the candidate against `HEAD`
      on `column_reader`
- [ ] 5.5 The prefetch hint of `relhash` on x86-64, `prefetcht0`, in
      `table/lanes.rs`, with the design of `relhash` and the forecast
      of its contract. Check: `cargo test --locked -p relhash`; the
      x86 listing of every program against `HEAD`, for the rule "No
      cost to Tessera"; as root, twice, the candidate against `HEAD`
      on `table_int32` and `table_large`

## 6. The close

- [ ] 6.1 Write what this design decided, and why, into the
      documentation of `simd` and of the crate, where the vector code
      is described, without dates or references to the change; the
      final words of `docs/limitations.md`. Check:
      `RUSTDOCFLAGS=-Dwarnings cargo doc --locked -p tessera-kernels
      --no-deps`
- [ ] 6.2 Close the change: delete its folder. In the roadmap:
      - the entry leaves the queue and its text;
      - `bench-in-four-cells` loses the run on the Intel Mac and its
        texts;
      - `linux-x86-support` no longer waits for vector kernels;
      - `oltp-guard-bench` and `prosupport-batch-functions`, which are
        ordered after this entry, are ordered anew;
      - `avx-512` gets what AVX2 showed;
      - a new entry takes the losses of the row paths of x86-64 to the
        drivers' decision for whole words, which the pilot measured,
        with the two shapes of the loops of `count` and the aggregates
        and `as_chunks` for the bounds of `word_slots`, which 4.8 left;
      - "Not placed" gets the random cases of the benchmarks, which
        give the same words every call, so that the predictor learns
        them and a verdict depends on where the code lies;
      - each finding the maintainer has not decided on goes to "Not
        placed".

      Check: `.github/scripts/check-specs.sh`; no line of the roadmap
      names `simd-primitives-avx2`
- [ ] 6.3 Open the second pull request, which closes the change, with
      the outcome under Decision as `CONTRIBUTING.md`, "Closing a
      change", asks. Check: every item of its description points to a
      test, a command or the id of a run
