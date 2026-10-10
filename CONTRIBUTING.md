# Contributing

The rules of work on Tessera, for people and coding agents alike. They
are written here and nowhere else: [AGENTS.md](AGENTS.md), the file
coding agents load by themselves, only points to this one.

## Getting started

Build Tessera as the [README](README.md) shows. Any change beyond a
small fix goes through
[OpenSpec](https://github.com/Fission-AI/OpenSpec): what a part promises
is a spec, and a piece of work is a change with its proposal, design and
tasks. "When work needs an OpenSpec change" below draws the line. The
skills that teach a coding agent this workflow are not kept in the
repository. Install the CLI and let it write them, once in every clone
and worktree:

```sh
npm install -g @fission-ai/openspec
openspec init --tools claude,codex
```

`openspec/` is already here, so `init` only writes the skills: under
`.claude/` for Claude Code and under `.agents/` for Codex, both of which
git ignores. It leaves the project's settings as they are. `--tools`
takes the name of any agent OpenSpec knows. CI checks the specs with the
version of OpenSpec named in
[ci.yml](.github/workflows/ci.yml); install that one if a check
disagrees with yours.

## Where the knowledge is

Read in this order, and stop when the question is answered.

1. [README.md](README.md): what Tessera does and what runs in batches.
2. `openspec/specs/<capability>/`: one folder for each part of the
   system. `spec.md` says what the part promises, and every scenario
   names the test that shows it. `design.md` says how the part is built
   and why, from the whole to the details. `contract.md`, where a part
   has one, holds the rules of its build (see "Architecture
   contracts"). The parts are being described one by one: a part
   without a folder is still described in `docs/`.
3. [docs/](docs): guides for authors of extensions on top of Tessera
   (bridge, node contract, writing a node, functions, kernels, runtime,
   sources) and, until they move to their capabilities, the design of
   the nodes, the hash table, the spill and the costs.
4. `openspec/changes/`: work in progress, each change with its proposal,
   design, tasks and spec changes. A finished change leaves no folder
   behind: see "Closing a change".
5. [openspec/roadmap.md](openspec/roadmap.md): the queue of work, what
   is not built yet and in which order, and what was decided against.
6. [docs/plan/](docs/plan/README.md): the working log in Russian of how
   the code came to be, frozen on 2026-10-04. Its
   [index](docs/plan/INDEX.md) lists the items by part of the system.

Where a spec and another text disagree, the spec is right. Where a spec
and the code or a test disagree, do not choose silently: see "Stop and
ask".

## The repository

- `include/tessera/`: the public C API — batches, masks, bindings,
  registries, the kernels' ABI.
- `bridge/`: the `tessera` extension, one API table and the registries.
- `runtime/`: the static library every node links.
- `nodes/`: `tessera_nodes`, the executor nodes, their planners and the
  settings.
- `kernels/`: `tessera_kernels`, batch functions, linked with the Rust
  kernels.
- `crates/`: the Rust kernels (`tessera-kernels`), their C entry points
  (`tessera-capi`), the spill format, the shared core.
- `tools/`: measuring and checking tools: `tessera-ab`, `tessera-bench`,
  `tessera-crosscheck`, `tessera-tpch`.
- `test/`: the SQL suites with their C test modules; `bench/`: the
  benchmark families.

## Building and checking

Before pushing, run what CI runs. `PG_CONFIG` names a PostgreSQL master
build with assertions and a UTF-8 locale (CI uses `C.UTF-8`).

```sh
make rust-check                  # format, Clippy, debug and release tests
make && make install
make installcheck EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"
.github/scripts/check-specs.sh   # the specs against the repository
```

`make rust-loom` runs the model of the shared hash table, `make
crosscheck` random queries with Tessera on and off. CI runs all of these
and more ([ci.yml](.github/workflows/ci.yml)), and the mutants of a pull
request's changed lines ([mutants.yml](.github/workflows/mutants.yml)):
only Rust tests judge a mutant, so an entry point needs a Rust test of
its own.

Before a result is copied into `test/expected/`, read its diff: every
`ERROR`, every removed line. The expected file is the oracle.

## When work needs an OpenSpec change

A change (`/opsx:propose` in Claude Code, `$openspec-propose` in Codex)
is required when

- behavior, the C API or a format changes;
- work on speed needs a measurement plan;
- the work is larger than one pull request.

Anything else is an ordinary pull request. The maintainer approves
twice: the proposal before any code, and the outcome before the change
is closed. A discussion of what a feature would take ends in an edit of
a change or of the roadmap, not in code.

Existing code gets its spec the same way, one capability in a pull
request, at most ten requirements, with the text of its design moved to
`design.md` in a commit of its own. The Decision of such a pull
request lists every disagreement of the documents, the code and the
tests, and every promise without a test.

## Closing a change

A finished change is not kept in the tree: git has its history, and a
second copy would only go stale. The commit that closes a change puts
what must outlive it in its place and deletes its folder.

- The requirements go to `openspec/specs/`: `openspec archive <name>`
  writes them there. The folder it makes under
  `openspec/changes/archive/` is deleted in the same commit.
- What was decided about how a part is built, and why, is written into
  that part's `design.md` as part of its text.
- What is left undone becomes an entry of `openspec/roadmap.md`, and a
  finding the maintainer has not decided on goes to its list "Not
  placed"; the change's own entry leaves the roadmap.
- A full run of a benchmark is kept as
  [docs/measuring.md](docs/measuring.md) says.

The outcome the maintainer approves is the description of the pull
request that closes the change: what was done, what was measured, with
the ids of the runs, where the work departed from the design and why,
the findings, and what is left.

## Specs

- A spec names four surfaces only: the public C API; behavior visible
  from SQL (the plan chosen, results, EXPLAIN, settings, SQLSTATE);
  formats on disk and in shared memory; the kernels' entry points.
  Internal names and steps go to `design.md`.
- One promise in a requirement. The rules of a format (its fields,
  codes, widths and limits), the drawings of its layouts, the lists of
  what is supported and the error codes are written once, in the spec.
  A design explains the idea, with a picture of the whole or an
  example, and points to the spec for a rule or a layout; it does not
  repeat them.
- Every scenario ends with the test that shows it:
  ``- **Verified by:** `path::text` ``, where the text occurs in the
  file: a test's name, or a mark `-- spec: <capability>/<scenario>` in
  an SQL suite. `review only — <reason>` when no test can show it;
  `pending` only in an open change.
- A spec, a design and a contract are written for a reader who knows
  nothing about the part and wants to understand it, in simple English:
  short sentences, common words, one thought in a sentence. They explain
  every term where it first appears. In a spec a layout of data is a
  drawing in a code block, with its offsets and sizes.
- `design.md` goes from the whole to the details: what the part is for
  and the problem it solves; the engineering goals and what each choice
  costs; the design as a whole, with a picture; then each piece, with
  the alternatives that were refused. `## Files` and `## Tests` close
  it; the check fails when a file it names is gone.
- All three are self-contained. They outlive the working plan, the
  roadmap, the changes and the pull requests, so they cite none of them
  and give no dates: what a reader needs is written in the document
  itself. The check fails on such a reference.

## Architecture contracts

A spec says what a part promises from outside. A part also keeps rules
of its own build that no test of its behavior shows: the crates it may
depend on, the one way to its memory, what its hot calls must not do.
Code can pass every test and break such a rule, and a series of pull
requests, each sound on its own, can wear a boundary away. A part
writes these rules as its architecture contract, a third file of its
folder, `contract.md`, beside `spec.md` and `design.md`.

- A rule protects a property and says why it matters, not where the
  code lies. "The probe of a table can change without a change of the
  join's semantics or of the PostgreSQL layer" is a rule; "the join
  lives in `join/hash.rs`" is not. A rule forbids what would be wrong
  and leaves the right structure to the work. One rule, one sentence
  and its reason.
- Each rule ends with how it is checked. Most rules are checked by CI.
  - ``- **Checked by:** `path::text` ``: a check that runs in CI, named
    as a "Verified by" names a test.
  - `probe — <the change>`: the change is made on a branch that is never
    merged, and the parts and interfaces it touched are counted. A probe
    runs when the part's boundaries change, and before a release.
  - `agent review — <a procedure and what it reports>`: the reviewer
    does something whose result can be seen, such as listing every
    module and interface a named change would touch. "Check that the
    design is clean" is not a procedure.
  - `maintainer — <the question>`.
  - `pending`, only in an open change.
- A contract also names the changes the part is likely to see, in its
  own words, naming no entry of the roadmap. For each it names the
  parts and interfaces the change would touch. The pull request that
  makes such a change compares what it touched with this forecast, and
  updates the forecast.
- A change that touches a part or an interface its forecast did not
  name, or that adds a dependency between parts, says why under
  "Architecture". Each such deviation goes into an entry of the roadmap
  for simplifying that part, which the first one creates, and names the
  boundary it crossed. A forecast is a guess, so two deviations over
  one boundary call for a diagnosis, not a rework: the next pull
  request on the part first finds whether that boundary has worn away.
  If it has, the pull request simplifies before it adds anything; if
  not, it corrects the forecast and says on what evidence.
- A part's responsibilities are the invariants it keeps. When a part
  takes on a new one, the pull request says under "Architecture" why
  the part keeps it rather than a new part or a neighbor. One invariant
  has one owner: a rule kept in several parts, such as one rule of
  eviction written in three nodes, means its owner is missing.
- A change that breaks a rule is reworked until it keeps it. When no
  rework keeps it, the agent may propose to change the rule, in a pull
  request that changes only the contract. The proposal says what the
  rule costs, which ways were weighed, why the work cannot keep it,
  and what is kept or replaced. The maintainer decides. An exception
  for a while is such a pull request too: it narrows the rule and says
  when the narrowing ends, and its reason goes into the part's
  `design.md` as a decision, with what was, what will be and why.
- The checks of a contract read it from the base of a pull request,
  not from its head, so a change cannot loosen the rule it is judged
  by. `check-specs.sh` does not read contracts yet. The pull request
  that adds the first `contract.md` also adds these checks: the
  "Checked by" of every rule, and the reading from the base.
- When the maintainer has to set a part's architecture right by hand,
  the class of the fault is named, and the cheapest reusable means
  that would have caught it earlier is added: a check, a probe, a
  forecast or a sharper rule. A rule is added for a new class of
  fault, not for each feature, so a contract stays short.

## Documents

Everything in the repository is written in English. Markdown is wrapped
at 72 columns and never runs past 80, so that it reads in a terminal, in
a diff and in two panes side by side. This holds for specs too: a
requirement, a scenario's bullet and its "Verified by" continue on the
next lines. What does not fit a line of a table is not a table: write a
list. A code block, and a link or a path that cannot be broken, may run
longer. `check-specs.sh` checks this file, `AGENTS.md` and `openspec/`.

## Commits and pull requests

- A series of commits goes on a branch of its own and reaches `main`
  through a pull request into `main`: CI green, the maintainer's review,
  the maintainer's merge. Nothing is committed to `main` directly.
- A commit is one change that builds and passes the checks by itself.
  Mechanical moves and renames are commits of their own. A commit with
  more than about 500 lines of production code, tests and documentation
  aside, says in its message why it is not split.
- The message says why the change is made and reads on its own; lines
  of 72 characters; a `Validation:` paragraph lists the checks run, as
  commands, without numbers. It may name the OpenSpec change it belongs
  to, whose proposal and design git keeps after the change is closed;
  older commits cite items of the
  [working plan](docs/plan/README.md). No attribution lines of tools or
  agents in messages or pull requests.
- The description of a pull request is an index of evidence. Each item
  of Behavior, Safety, Architecture and Performance points to something
  a tool made: a test, a command and its log, the id of a run. The
  sections:
  - **Behavior**: the tests that show it, and what they compare with;
  - **Safety**: Miri, loom, the sanitizers or a proof, for unsafe code,
    the C boundary and concurrency;
  - **Architecture**: what the change did to dependencies, the public
    API, unsafe code and the rules of a contract, and what it touched
    against the forecast;
  - **Performance**: the machine code, the counters or the times
    against the base, when a hot path changes, with every way tried,
    the failed ones too;
  - **Residual risks**: what was not checked, and why;
  - **Decision**: why the change can be taken; each trade-off settled
    without the maintainer, with the rule that settles it; and what the
    maintainer must decide.

  An item may point to a commit's `Validation:` paragraph instead of
  repeating its commands. A pull request without code gives the index
  in a line. The pull request that closes a change adds, under
  Decision, what "Closing a change" asks.
- A commit brings with it: comments on new public types and functions
  (ownership, lifetime, errors, concurrent access), tests, a comparison
  of the native path with the Datum path for a new type or operation,
  and a measurement when a hot path changes.

## Measurements

[docs/measuring.md](docs/measuring.md) holds the rules. In short: time
only a release build; make sure the machine is idle before any load, and
an agent asks the person it works for; five minutes a run at most; the
tools, not scripts for the occasion; evidence before an optimization is
proposed.

## Stop and ask

An agent decides alone what the specs, the contracts and these rules
settle: the structure of the code and the ownership of memory inside a
part, the choice between ways that pass the same checks, the fix of a
review's finding that they settle, a test to add. A finding they do not
settle goes to "Not placed" in the roadmap.

When a way fails a check, such as a loop that compiles longer without a
reason or a confirmed excess over a threshold, the agent goes on while
each new try improves the failing measure or rules out a cause. Each
try is a hypothesis written down before it is tried, never a rerun of
the same way. The search ends after two tries in a row that do
neither, or after about an hour. A failure that a rule of a contract
causes is not searched around: the rule is the question.

The agent stops and asks, with the evidence and the ways it weighed,
when:

- A fix changes a boundary: a public API (the C API, behavior seen from
  SQL, the public API of a crate), the ownership of memory across the C
  boundary or between parts, or a rule of a contract.
- Ways differ by a trade-off that no rule settles.
- The search above ends without a way that passes.
- A risk to query results or to data remains that no check can close.
- A spec, a design, a document, the code and a test disagree.
- An action cannot be undone: deleting a branch or a cluster, rewriting
  pushed history, merging.

## Accepted limits

- PostgreSQL master is the supported core; Greengage 7 is planned.
- The ABI does not tie a PostgreSQL type to one physical format of a
  column.
- For now performance counters work only on macOS with Apple silicon;
  x86 and Linux are planned. CI checks correctness, not speed.
