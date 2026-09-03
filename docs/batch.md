# Batches

`TessBatch` is a format-neutral envelope around one group of physical rows.
The producer chooses the physical column format and exposes it through
`TessBatchOps`. Every format must provide a lazy conversion to PostgreSQL
`Datum` arrays, so independently built consumers always have a common path.

The `rows` mask identifies the rows that remain active. A consumer may clear
bits but must not restore rows, change physical columns, or modify the
operation table and private data.

## Column access

`get_datum_column` receives a zero-based logical column number and a row mask.
The requested mask has the same physical row count as the batch and is a
subset of its active rows. A provider must materialize the requested rows and
may materialize more rows as an optimization.

The caller initializes `TessDatumColumn.struct_size`. The callback fills
`values`, `isnull`, and `nrows` without changing that size. Both arrays use
physical row numbers. `isnull` contains one `bool` per row and is not a bitmap;
`values[row]` is undefined when `isnull[row]` is true.

`TessColumnPurpose` tells the provider whether the values are needed for a
filter or a later projection. It may affect preparation strategy but never the
values returned.

The returned arrays, including pass-by-reference Datum values, remain valid
until the batch is released. Views of several columns may be used together.
The provider must not retain the callback's `rows` or `result` pointers.

## Ownership

The producer owns the batch and its storage. Only one node may use the batch
at a time. `TessApi.publish_batch()` transfers exclusive use to a slot
binding, and the producer must then stop using it. `TessApi.release_batch()`
returns it to the producer and calls the optional `release` callback. Without
that callback, the producer must need no separate cleanup for the batch.
After release, the former consumer must not access the batch again.

There is no reference counting or concurrent access. A batch is local to one
PostgreSQL backend. Parallel workers use separate batch objects.
