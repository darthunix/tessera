## Context

`TessAgg` in a parallel plan is two nodes. The partial one runs in
every participant and keeps a table of its own groups. Past seven
eighths of `hash_mem` the table either goes up, its groups sent to the
final node as partial values, or, when it folds too little, spills its
records to disk and goes up once the input is done. The final node
above `TessGather` merges what comes up.

A key that a word does not hold is a number in the table, given by a
dictionary of the node's values. The planner's rule `partial_keys`
kept such keys out of the partial stage, for the reason its comment
gave: a partial table empties early, and the dictionaries would go
with it. The executor settled this in another way. A key through a
dictionary marks the grouping as one whose records cannot spill, and
in the partial stage such a grouping never spills: its table goes up
every time it fills. The dictionary starts anew when the next table is
made, after the batch that carried the last groups up was released.

## Goals / Non-Goals

**Goals:**

- One rule of which keys a partial grouping takes, the serial node's,
  for both cores.
- The node's partial stage for text and numeric keys under a `Gather`,
  with no change of results.

**Non-Goals:**

- A cheaper dictionary for `char(n)`.
- A new cost of the partial stage: the cost of a key through a
  dictionary (`tessera.agg_dictionary_share`) is already the serial
  node's and the partial node's.

## Decisions

### Remove the rule, not narrow it

The rule goes from all three places, and with it the function. Lifting
it only where Greengage builds its stages would keep two rules of
which keys a partial grouping takes, one per core, for a fault that
neither core can meet.

### A table too full: one function, a floor for each stage

Each dictionary used to take room for the groups expected, up to a
quarter of `hash_mem`, and the empty table up to three eighths more.
Three or four dictionary keys and a large estimate passed seven eighths
of `hash_mem` before a row was read: the partial table went up empty,
was made anew of the same size, and went up again, forever. Two
dictionary keys came near it, and the table went up after every batch.
The dictionaries of a node's keys now start with room for one quarter
of `hash_mem` together, 256 values each at least; it is where they
start, not a bound, and their values grow as the groups come. One
function, `groups_overflow`, decides both when a serial table freezes
and when a partial table goes up, each with its own floor: a serial
table freezes with a batch's groups at least, as before, since frozen
smaller it would split every partition again; a partial table goes up
with one group at least, since an empty one past the limit is only
the room taken for the groups expected. The floor of one keeps the
early emits of a few groups whose states are large, such as numeric
sums of long rests. The share also helps a serial grouping by several
text keys, which froze at 64 groups for the same reason.

The other way was to keep the planner's rule for three keys or more:
it would leave the fault for two keys, and two owners of what a full
table is.

### No new cost

A partial table of a key with many values may fold nothing before it
fills, and then hands the final node almost every row. Sum states do
the same today. The cost of the partial stage counts the dictionary's
lookups but not its memory, so the planner's point where the groups
pass `hash_mem` is optimistic for such keys; this change leaves that
as it is and the close records it.

## Risks / Trade-offs

- A grouping whose estimate of the groups is far too low takes the
  partial stack, and its tables fold nothing: the final node merges
  every row once more. The same holds for sum states and word keys
  past `hash_mem`. The tests check the results of such tables, not
  their time.
- `numeric` keys: equal values of different scales are one group, and
  the group shows the form of the first row its final node meets, as
  the core's own two stages do. The tests compare such groups after
  `trim_scale`.

## Measurement

`cargo ab measure --base main --family anykey --workers 2`, base and
candidate alternating, on the bench cluster at scale 1 with a release
build of PostgreSQL master; then `cargo tpch check --workers 2` on
both sides, and `cargo tpch --queries` of the queries whose plans
changed, with Q6 as a control, compared with `cargo tpch compare`. A
case slower beyond the tool's threshold stops the work for the
maintainer.
