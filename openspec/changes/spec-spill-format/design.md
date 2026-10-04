## Context

`docs/spill.md` has six sections. Three describe the format and the
files ("What is written", "The block header", "Files", 137 lines); three
describe what TessHashJoin and TessAgg do with them ("In the hash join",
"In the grouping", "The weights", 233 lines). The code of the format is
the crate `tessera-spill`, its C entry points in `tessera-capi`, and
`runtime/spill.c` for the files.

## Goals / Non-Goals

**Goals:**
- One place that says what a block is, byte by byte, and what a reader refuses.
- Each statement tied to a test by name, so that CI fails when the test goes.
- The promises without a test visible as such.

**Non-Goals:**
- The policy of spilling: when a node spills, how it partitions, what it
  keeps in memory, the weights. They belong to the capabilities of the
  join and of the grouping.
- The partition plan (`tess_spill_partitions`, `tess_spill_chunk_len`)
  and the append of rows to a chunk of columns beyond the layout it
  fills.
- Fixing what the comparison found. The change records it; the
  maintainer decides.

## Decisions

### What goes to the spec and what to the design

**Was:** the header's diagram, the list of checks, the kinds and the
packed forms described in prose in `docs/spill.md`, and again in
comments.

**Will be:** `spec.md` holds every definition a reader of the bytes
needs: the table of the header, the lengths allowed for each kind, the
codes of a packed chunk of records, the descriptor of a packed lane of
columns, the lists and the trailer of a shared file. `design.md` holds
why it is so: why chunks are written whole, why records are packed by
lanes and not compressed, why one file a set and not one a partition.
The design does not repeat a definition; it points to the spec.

### Four surfaces

The spec names the bytes on disk and the public C API
(`tessera/spill.h`, `tessera/runtime_spill.h`). The Rust functions
behind the entry points are named only as the place of a test.

### A promise without a test

**Was:** not visible.

**Will be:** the scenario is written and ends with `review only` and the
reason. Four scenarios do: the lengths of a header of kind 3, the limits
of a chunk of columns, a shared file without its lists, and a block that
is not where the list says. `check-specs.sh` prints them on every run.

### What we do not do

- No test is added here. Whether a promise gets a test or leaves the
  spec is the maintainer's choice, one promise at a time.
- No comment in the code is corrected here, though five name two kinds
  of block: this change moves text and writes the spec.
- The split of `docs/spill.md` is by section, not by sentence: a
  sentence about the join inside a format section moves with its
  section.
