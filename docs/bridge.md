# Tessera bridge

The Tessera bridge is a small backend-local service that lets independently
built PostgreSQL extensions find one shared API table. It uses PostgreSQL's
rendezvous variables, so components do not link directly to one another.

The bridge owns only cross-extension coordination:

- attaching a batch request to a tuple slot;
- transferring one active batch through that slot;
- registering batch sources;
- registering batch-producing node kinds.

It will not evaluate expressions, interpret a source-native column format,
choose scan or join algorithms, or own the physical buffers behind a batch.
Those responsibilities belong to runtime libraries, nodes, and sources.

## Build and run the example

The complete example is the independently built
[producer](../test/tessera_producer_test.c) and
[consumer](../test/tessera_consumer_test.c) from step 2.10. The
[SQL scenario](../test/sql/modules.sql) loads both and runs a transfer using
only public Tessera interfaces. The walkthrough below explains selected
fragments; the linked C files contain the complete, compiled implementation.

Use an installed PostgreSQL master build with server headers, PGXS, and its
server and regression-test programs. You also need a C compiler and `make`.
Choose that installation's `pg_config`, and run these commands from the
Tessera repository root:

```sh
TESS_PG_CONFIG=/path/to/postgresql/bin/pg_config
TESS_TEST_PORT=56437

make clean PG_CONFIG="$TESS_PG_CONFIG"
make PG_CONFIG="$TESS_PG_CONFIG"
make install PG_CONFIG="$TESS_PG_CONFIG"
```

Installation requires write access to the selected PostgreSQL installation.
It installs the bridge, extension files, and public headers. The test
libraries remain in the build's `test` directory; they are not installed.
Always clean and rebuild when switching PostgreSQL installations, including
between builds with and without assertions.

Choose a free port and run the example in a fresh temporary instance:

```sh
TESS_TEST_TMP=$(mktemp -d)
make installcheck PG_CONFIG="$TESS_PG_CONFIG" REGRESS=modules \
  EXTRA_REGRESS_OPTS="--temp-instance='$TESS_TEST_TMP/cluster' \
    --outputdir='$TESS_TEST_TMP/results' \
    --port=$TESS_TEST_PORT"
```

Run this as an ordinary operating-system user, not `root`: PostgreSQL's
`initdb` refuses to run as root. The temporary instance supplies the database
superuser needed to load C libraries and create the test functions. It does
not use an existing database server. The regression runner supplies the test
library directory and platform suffix through `PG_LIBDIR` and `PG_DLSUFFIX`;
the SQL file is intended to run through that runner, not directly via `psql`.

Expect the `modules` scenario to pass. Its expected output includes two
intentional errors: an invalid consumer function signature and a consumer
failure after column access. A successful transfer before and after those
errors returns `t`. See the [expected output](../test/expected/modules.out).
The runner stops its server after the run and leaves results and logs under
`$TESS_TEST_TMP/results`, including `regression.diffs` on a mismatch.

To run all nine scenarios, use another fresh directory and omit `REGRESS`:

```sh
TESS_ALL_TEST_TMP=$(mktemp -d)
make installcheck PG_CONFIG="$TESS_PG_CONFIG" \
  EXTRA_REGRESS_OPTS="--temp-instance='$TESS_ALL_TEST_TMP/cluster' \
    --outputdir='$TESS_ALL_TEST_TMP/results' \
    --port=$TESS_TEST_PORT"
```

## Load the bridge in each backend

The extension is named `tessera`. Its creation script loads the shared
library, whose `_PG_init()` publishes `TessApi` under `tessera.api.v0`:

```sql
CREATE EXTENSION tessera;
```

This loads the bridge in the backend executing that command. The extension's
presence in the database does not load the library in a new connection.
Before a component looks up the API in another backend, arrange to load it
there; an explicit SQL load is:

```sql
LOAD '$libdir/tessera';
```

`CREATE EXTENSION IF NOT EXISTS tessera` is not a substitute when the
extension already exists: its creation script will not run again. Loading
the same library again in an already initialized backend is safe.
`find_rendezvous_variable` only finds or creates the rendezvous location;
it does not load Tessera. An empty location means the bridge is unavailable
in that backend, even if the extension exists in the database.

Each parallel worker has its own backend-local API and registries. A batch
or binding pointer cannot be shared with another backend. There is no
reference counting or concurrent access to a batch.

## Subsystem tables

`TessApi` remains a small root containing pointers to independently versioned
subsystem tables. `TessBindingOps` implements the slot binding protocol
described in [binding.md](binding.md). `TessSourceRegistryOps` implements the
source registry described in [source.md](source.md). `TessNodeRegistryOps`
implements the node registry described in [node.md](node.md).

The current root requires `binding_ops`, `sources`, and `nodes` to be non-null.
A consumer first checks the root's ABI version and `TESS_API_MIN_SIZE`, which
includes all three fields. Before using a subsystem, it then checks that
table's pointer, ABI version, and minimum size.

Future optional fields can be appended to the root. A consumer checks
`TESS_ABI_HAS_FIELD` before reading such a field, then validates the subsystem
table itself. The current required fields are covered by `TESS_API_MIN_SIZE`
and do not need separate field checks.

After loading the bridge, the [producer example](../test/tessera_producer_test.c)
uses a local helper to obtain and validate the slot-binding table:

```c
const TessBindingOps *ops = producer_binding_ops();
```

`producer_binding_ops` belongs to the example, not the public Tessera API.
Its full implementation checks the headers and required pointers described
above, plus the callbacks it will use. The consumer performs its own lookup
and validation in `consumer_binding_ops`. API discovery uses `postgres.h`,
`fmgr.h`, and `tessera/bridge.h`.

Keep the validated operation table for subsequent calls; do not repeat API
discovery for each row or batch. The source and node registries currently
describe identities, not planning or execution callbacks. Their usage is
covered in [source.md](source.md) and [node.md](node.md).

## Version zero

`TessApi` and every subsystem operation table begin with an ABI version and
their actual structure size. A version changes when that contract becomes
incompatible. The size lets a consumer check whether a later field is present
before reading it, so compatible operations can be appended without breaking
an older provider.

All public size-tagged structures follow one rule: existing fields keep their
position, type, and meaning, while compatible fields are appended at the end.

`TESS_ABI_SIZE_INCLUDING_FIELD(type, field)` computes the size in bytes from
the start of the structure to the end of `field`, including that field. Each
`TESS_*_MIN_SIZE` definition selects the last required field for its structure.
`TESS_ABI_HAS_FIELD` uses the same calculation to check whether a supplied
structure includes a field, including an optional field, before use.

The two initializer macros fill either a version-and-size header or a
size-only header, so each module reports the structure layout against which
it was compiled.

Version zero is an experimental development ABI, not a compatibility promise.
The first public release will assign a stable nonzero ABI version after the
interfaces have been reviewed together.

## Walkthrough: request, transfer, and cleanup

The producer's `tessera_test_modules(regprocedure)` accepts the identity of
a consumer function with signature `(internal, boolean) RETURNS boolean`.
It validates that signature, then calls the consumer through PostgreSQL's
`fmgr`. The `internal` argument carries a `TupleTableSlot *`; the boolean
selects preparation (`true`) or consumption (`false`). SQL never receives
the pointer. This is test-only invocation machinery, not a Tessera API or
a mechanism for planning SQL queries.

The consumer retains its own state in `FmgrInfo.fn_extra`, allocated in the
caller's function context. It does not know the producer's storage structure
or inspect `TessBatch.private_data`. Neither module links to the other or
to the bridge's object files.

The sequence for one connection is:

```text
Producer                               Consumer
attach(slot, layout) -----------------> find(slot), retain binding
                            <--------- set_request(binding, request)
freeze_request(binding)
publish_batch(binding, batch) --------> get_batch(binding)
                                       read filter column, remove rows
                                       read projection column
                            <--------- mark_consumed(binding)
is_consumed(binding)
release_batch(binding)
        ... repeat publication and consumption for the next batch ...
detach(binding)
destroy slot and remaining test state
```

### Prepare the connection

In `producer_run_case`, the producer creates a virtual tuple slot with an
`int4` and a `text` attribute, and attaches an identity layout:

```c
TessLayout layout = {
    .struct_size = sizeof(TessLayout),
    .ncolumns = 2,
    .ntargets = 2,
};

state->binding = state->ops->attach(state->slot, &layout);
```

`TessLayout` maps target positions to logical batch columns; it does not
describe types or physical buffers. Here the types come from the slot's
`TupleDesc`. The bridge copies the layout and any target map into the slot's
memory context.

During `consumer_prepare`, the consumer calls `find(slot)` once and retains
the binding. It checks the layout and requests column 0 for filtering and
column 1 for projection:

```c
TessRequest request = {
    .struct_size = sizeof(TessRequest),
    .output_mode = TESS_OUTPUT_BATCH,
    .max_batch_rows = 70,
};

request.filter_columns = bms_make_singleton(0);
request.projection_columns = bms_make_singleton(1);
state->ops->set_request(state->binding, &request);
bms_free((Bitmapset *) request.filter_columns);
bms_free((Bitmapset *) request.projection_columns);
```

`set_request` copies the masks, so the caller can immediately free its
originals. The producer then calls `freeze_request` and checks the received
requirements. Freezing is permanent for that binding; repeating it is safe.
Publishing also freezes the request if it has not already been frozen.
`max_batch_rows` limits physical rows in each batch, not selected rows or the
total query result; zero means no explicit limit. See [request.md](request.md).

### Publish and read a batch

`producer_make_batch` creates the envelope, row mask, and column storage in
a dedicated memory context. Physical row `r` in batch `s` has integer value
`1000 * s + r + 1` and text label `row-N` for that value. The integer is null
when `r % 5 == 0`; the text is null when `r % 7 == 0`. Values are prepared
only when the consumer requests their rows. This known data lets the test
verify results without exposing the producer's physical representation.

The producer transfers exclusive use with `publish_batch` and stops using
the batch's contents. In `consumer_read`, the consumer obtains the borrowed
batch, validates its ABI and operation table, and requests the filter column:

```c
TessDatumColumn numbers = TESS_STRUCT_INITIALIZER(TessDatumColumn);

batch->ops->get_datum_column(batch, 0, &original,
    TESS_COLUMN_FOR_FILTER, &numbers);
```

Here `original` is the consumer's copy of the initial active row mask.
Each column request must use the same physical row count as the batch and
select only currently active rows. The result's `struct_size` belongs to
the caller and must be preserved by the provider.

`numbers.values[row]` and `numbers.isnull[row]` use physical row numbers,
not packed positions in the selection. `isnull` contains a `bool` per row;
it is not a bitmap. Never read `values[row]` when that row is null, and do
not assume unrequested rows have been prepared.

The consumer removes rows with null or odd integers using
`tess_row_mask_clear`. It then requests text only for survivors:

```c
TessDatumColumn labels = TESS_STRUCT_INITIALIZER(TessDatumColumn);

batch->ops->get_datum_column(batch, 1, &batch->rows,
    TESS_COLUMN_FOR_PROJECTION, &labels);
```

The test verifies text values and then rereads the original integer view,
including rows removed during filtering. Obtaining another column must not
invalidate an earlier view. In general a provider may prepare extra rows as
an optimization; this test provider prepares only the requested rows.
The provider must not retain the callback's `rows` or `result` pointers.
See [batch.md](batch.md) and [row-mask.md](row-mask.md).

### Finish consumption, then release

The consumer's final batch operation is:

```c
state->ops->mark_consumed(state->binding);
```

It must not access that batch or its column views again. This operation
changes only the binding state: it neither removes the batch nor frees its
memory. The producer checks `is_consumed`, then calls `release_batch` before
publishing another batch. A consumed but unreleased batch still occupies
the binding and prevents publication.

`release_batch` clears the active pointer before calling the optional
`TessBatchOps.release`. The test's `producer_release` destroys the batch's
memory context, including its envelope, arrays, and text values. Counters
outside that context verify exactly one release, even when `release_batch`
is repeated. A provider without a release callback must not require that
callback for cleanup; its ownership obligations do not transfer to the bridge.

The example sends two batches with different values through each binding.
It covers sizes 1, 3, 63, 64, 65, and 70, plus sparse and empty selections.
An empty selection with positive physical row count is a valid active batch,
not an absent batch or automatic completion. A zero-physical-row batch cannot
be published. These operations do not populate the slot's visible scalar
row; scalar execution adapters are separate work.

## Ownership and pointer lifetime

All objects below are local to one backend. A borrowed pointer must not be
freed by its recipient and does not extend its owner's lifetime.

| Object | Owner | Lifetime and caller obligations |
| --- | --- | --- |
| `TessApi` and subsystem tables | Bridge | Valid for the backend lifetime after loading; retain validated pointers. |
| `TupleTableSlot` | Producer in this example | Call `detach` before destroying it. |
| `TessBinding`, copied layout and frozen request | Bridge, in the slot context | Borrowed results expire on `detach` or slot-context reset/deletion. Input layout and request buffers remain caller-owned and may be freed after the copying calls. |
| `TessBatch` and its physical storage | Producer | Exclusive use passes to the binding on publication. The consumer stops accessing it at `mark_consumed`; release returns storage control to the producer and may free it. |
| Datum arrays and referenced values, including text | Producer | Storage remains valid until batch release; a consumer may use multiple views together only while it still owns exclusive use. |
| `TessBatchOps` | Producer | Must outlive every batch using it. The example uses a static table. |
| Registered source/node descriptions and names | Registering provider | Remain valid and unchanged through registration; consumers finish before removal. Removal does not free the objects, and borrowed lookup results must not be used afterward. |

The source and node registries have separate name spaces. Their detailed
registration and removal rules are in [source.md](source.md) and
[node.md](node.md). Slot ownership is detailed in [binding.md](binding.md).

## Errors and cleanup

The bridge reports invalid operations with PostgreSQL `ERROR`; there is no
status-code return to ignore. For example, it rejects incompatible ABI
headers, invalid layouts or requests, batches larger than the requested
limit, and invalid row masks. The producer's column callback can also raise
`ERROR`, so protect the whole transfer, including column access.

Some important distinctions are:

- An unavailable API is an empty rendezvous location; the module must check
  before dereferencing it. An incompatible version or required size must
  likewise be rejected before using the table.
- `find` returning `NULL` means no binding or registry entry was found.
  `get_batch` returning `NULL` means no active batch. Neither is an error.
- `set_request` after freezing, publishing while any batch remains attached,
  and marking an absent or already consumed batch are errors.
- Repeating `freeze_request` is safe. Releasing an empty binding or passing
  `NULL` to `release_batch` is safe. Do not confuse this with calling
  `detach` twice on the same pointer: the first detach frees the binding.

`producer_run_case` uses `PG_TRY`/`PG_FINALLY` to protect the connection.
Both normal completion and an error follow the same order: switch to a
still-live cleanup context, detach the binding, destroy the slot, and then
delete remaining owned contexts. `detach` releases an active batch even if
the consumer never marked it consumed. The test also handles a failure while
constructing an unpublished batch by freeing its context separately.

The error consumer deliberately raises `ERROR` after obtaining a column.
The producer's finalization verifies one release per published batch and
removal of the binding before the original error propagates. The next SQL
invocation performs a successful transfer with fresh state.

Do not rely on a slot-context reset as a substitute for this cleanup. The
bridge's reset callback only removes its lookup entry; it does not invoke
the batch's release callback because the producer's context might already
have been deleted. External resources must therefore have their own
memory-context callback or `ResourceOwner` protection. A standalone call to
`get_batch` or a registry lookup does not establish any such protection.
