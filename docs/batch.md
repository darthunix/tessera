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

A string column of `text`, `varchar` or `bytea` may come in a second form,
its bytes as the producer holds them. `get_bytes_column`, an optional
operation after `release`, fills a `TessBytesColumn` for the rows of a mask
and returns true, or returns false for a column the provider does not
offer so; a consumer asks through `tess_batch_bytes_column()`, which also
answers false for operations of a size without the field, and reads any
column that is not offered through `get_datum_column`, as every consumer
can. For each row asked that is not NULL, `data` points to the value's
bytes and `lengths` gives their count, without a header, never compressed
nor stored apart; where the producer holds the column through a
dictionary, it may give `indexes`, the row's index, 0 or more and below
`nvalues`, and `dictionary` the dictionary's identity, which it takes
from Tessera's API, so that a consumer may keep what it learned of an
index while the identity stays; a producer of a backend whose API lacks
the identity operation gives no dictionary, and an identity never passes
from one process to another. The bytes passed the column's rule, as a
Datum made for the row would: a row asked whose bytes fail it raises the
error that making its Datum would, and a row not asked never fails the
call. A provider fills only the fields the caller's `struct_size`
covers. The pointers stay
valid until the batch is released, whatever the consumer asks of the
batch in between. The spec of the capability `string-bytes` draws the
structure and holds the rules of the dictionary and its identity.

`TessColumnPurpose` tells the provider whether the values are needed for a
filter or a later projection. It may affect preparation strategy but never the
values returned.

`values_stored`, the envelope's last field, says that reading a column
evaluates no expression: the values are stored, or deformed from stored
tuples, so a read can fail only on a damaged value and has no side
effect. A batch that computes a column on request, as a projection's
does, leaves it false, and so does any batch of a size without the
field: a reader asks `tess_batch_values_stored()`. The producer sets it
on each batch it publishes, by what that batch's columns read; a node
that hands on its child's batch hands on the flag with it.

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
