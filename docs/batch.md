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
physical row numbers. `isnull` contains one `bool` per row and is not a bitmap.
When `isnull[row]` is true for a requested row, `values[row]` is initialized
but meaningless (PostgreSQL slots store `0`): readers may load it, as vector
code loading several rows at once does, and must not interpret it. Rows the
provider did not materialize hold meaningless values too: Tessera's providers
allocate their arrays zeroed and leave stale values behind, so every row is
initialized memory and a consumer passes the kernels no readiness mask
(`prepared = NULL`, see [kernels.md](kernels.md)); a provider that leaves
rows uninitialized must say so before a consumer may rely on it, and none
exists yet.

A numeric column may come as decimals. A consumer that reads them sets
`accept_decimals`; a provider that has them (a projection's numeric chain,
see [expr.md](expr.md)) then answers with `decimal_rows`, the rows whose
Datum holds not a numeric but the int64 value times `10^decimal_scale`,
the value's display scale, of at most 18 digits; the other rows hold
numerics. A provider that does not know decimals, or a consumer that did
not ask, leaves `decimal_rows` NULL. The fields follow `nrows`, so an array
of columns, as the kernels read one, has the size of the whole structure:
the Rust mirror (`DatumColumn`) carries them too.

`TessColumnPurpose` tells the provider whether the values are needed for a
filter or a later projection. It may affect preparation strategy but never the
values returned.

The returned arrays, including pass-by-reference Datum values, remain valid
until the batch is released. Views of several columns may be used together.
The provider must not retain the callback's `rows` or `result` pointers. A
provider may keep by-reference values in buffer pages it has pinned and
drop the pins in its `release` callback, as the runtime's heap batch does.

## Ownership

The producer owns the batch and its storage. Only one node may use the batch
at a time. `TessBindingOps.publish_batch()` transfers exclusive use to a slot
binding, and the producer must then stop using it. The consumer calls
`mark_consumed()` when it no longer needs the batch. The producer may then
call `release_batch()`, which returns control of the storage and calls the
optional `release` callback. Cleanup may release a batch before it is marked
consumed. Without the callback, the producer must need no separate cleanup.
After marking a batch consumed, the former consumer must not access it again.

There is no reference counting or concurrent access. A batch is local to one
PostgreSQL backend. Parallel workers use separate batch objects.
