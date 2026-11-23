# string-bytes: the architecture contract

The bytes form lets a consumer read a string column where its producer
keeps it, without a `Datum` made for each row. It crosses the boundary
between a producer's memory, the Parquet reader's in Rust among them,
and a consumer's in C, and it gives a second way to a result the
`Datum` path already gives. These rules keep that memory safe, keep the
check of the bytes in one place, and keep the results of the two ways
the same. Each rule says what must not happen and why; how the code is
laid out is left to the work.

## Rules

### No pointer of the form past its batch
The bytes a batch gives in the bytes form stay where they are until the
batch is released, whatever is read in between, and no consumer keeps a
pointer to them past the release, so that no read of the form touches
memory that a later read, a reset or an error has freed.
- **Checked by:** pending

### One owner of the check
A producer checks the bytes of a column by the column's rule before it
gives them in the bytes form, and no consumer checks them again or
gives them on unchecked, so that the rule is written once and no path
lets a string through that its column promised to check.
- **Checked by:** agent review — list every producer of the form and,
  for each, the place where it checks the bytes it gives and the
  column's rule it reads there, and every consumer that passes the form
  on; a byte given before its check, a check of a consumer, or a
  consumer that passes the form of a column it computed fails the rule

### The same results by either form
A query returns the same rows, and fails with the same SQLSTATE,
whichever form each of its consumers reads, so that a batch's offer of
the form never changes a result.
- **Checked by:** pending

### Structures that grow at their end
The structures by which one module gives the form to another, the
form's own, a batch's operations, Tessera's API and a batch function's
call, gain fields only at their end and are read by the size their
caller gives, so that a module built against an older header reads them
as it did.
- **Checked by:** `test/installed/layouts.c::TessBytesColumn is 56 bytes`

## Forecast

- **The heap source's form.** Pointers into the pages the heap batch
  pins, for the values stored in place: the heap batch and its
  operations; not the consumers, nor the structures of the form.
- **The form through TessAppend.** Its batches' operations pass the
  form of its children's batches on, their identities as they are:
  TessAppend's operations alone; not the producers, nor the consumers.
- **The form through a parallel plan's gather.** The leader gets a
  worker's rows as copies, not its batches: a view of the form over the
  copies, without dictionaries, since an identity stays in its process;
  the gather's batches and the copies' layout; not the producers.
- **Joins by a string's bytes.** The hash join's keys read the form:
  the join's key hashing; not the producers, nor the form.
- **Another columnar source, such as Arrow's own files.** Its node gives
  the form from its own arrays: that node and its reader; not the
  structures of the form, nor its consumers.
