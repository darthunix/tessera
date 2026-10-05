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
   and why, from the whole to the details. The parts are being described
   one by one: a part without a folder is still described in `docs/`.
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
`design.md` in a commit of its own. Such a pull request ends with
"Decisions needed": every disagreement of the documents, the code and
the tests, and every promise without a test.

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
- One promise in a requirement. Formats, lists of what is supported and
  error codes are defined in the spec. A design may draw a layout again
  to explain it; where the two differ, the spec is right.
- Every scenario ends with the test that shows it:
  ``- **Verified by:** `path::text` ``, where the text occurs in the
  file: a test's name, or a mark `-- spec: <capability>/<scenario>` in
  an SQL suite. `review only — <reason>` when no test can show it;
  `pending` only in an open change.
- A spec and a design are written for a reader who knows nothing about
  the part and wants to understand it, in simple English: short
  sentences, common words, one thought in a sentence. They explain every
  term where it first appears, and a layout of data is a drawing in a
  code block, with its offsets and sizes.
- `design.md` goes from the whole to the details: what the part is for
  and the problem it solves; the engineering goals and what each choice
  costs; the design as a whole, with a picture; then each piece, with
  the alternatives that were refused. `## Files` and `## Tests` close
  it; the check fails when a file it names is gone.
- Both are self-contained. They outlive the working plan, the roadmap,
  the changes and the pull requests, so they cite none of them and give
  no dates: what a reader needs is written in the document itself. The
  check fails on such a reference.

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

- A fix needs a new public API, another model of ownership or a new
  architecture.
- A measurement confirms an excess over a threshold.
- A spec, a design, a document, the code and a test disagree.
- An action cannot be undone: deleting a branch or a cluster, rewriting
  pushed history, merging.

## Accepted limits

- PostgreSQL master is the supported core; Greengage 7 is planned.
- The ABI does not tie a PostgreSQL type to one physical format of a
  column.
- For now performance counters work only on macOS with Apple silicon;
  x86 and Linux are planned. CI checks correctness, not speed.
