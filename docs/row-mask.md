# Row masks

`TessRowMask` identifies the physical rows that remain active in a batch.
`bits[row / 64]` contains the bit for row `row`; bit zero represents the first
row in each 64-bit chunk. Bits beyond `nrows` must be clear.

The mask borrows its `bits` array and never allocates or frees it. A mask with
zero rows may use a null pointer. A nonempty batch with no selected rows still
has an allocated, zero-filled array.

Only the current owner of a batch may clear or intersect its mask. The
operations are not atomic, and a mask must not be changed concurrently.

Use `-1` to begin an iteration:

```c
int row = -1;

while ((row = tess_row_mask_next(mask, row)) >= 0)
	process_row(row);
```

The inline operations trust these invariants. The bridge will validate an
incoming mask before publishing its containing batch.
