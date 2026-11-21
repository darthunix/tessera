# datum-arrow: the architecture contract

`datum-arrow` is the boundary between PostgreSQL's types and Arrow's, a
crate meant for use outside Tessera. These rules keep it free of the
code on either side, keep its plans honest about what a value may do,
and keep every conversion between Arrow's values and PostgreSQL's in
one place. Each rule says what must not happen and why; how the code is
laid out is left to the work.

## Rules

### Free of both sides' code
The crate depends on no crate of Tessera and on no code of PostgreSQL,
and of Apache Arrow on `arrow-schema` alone outside its tests, so that
it builds and ships by itself.
- **Checked by:** pending

### A plan names every failure
No conversion of a value fails in a way its plan does not name, a call
of a broken shape aside, which is refused as one, a write's output
buffers only once its values pass the check of their size, so that a
caller that handles the errors the plan names has handled every value.
- **Checked by:** pending

### One owner of the conversions
A value crosses between Arrow's form and PostgreSQL's only through this
crate: no other code of the workspace that reads or writes Arrow's data
converts a date, a time, an integer or a string itself, so that a
reader and a writer cannot drift apart.
- **Checked by:** agent review — list every module of the workspace
  that reads or writes Arrow's arrays or Parquet's files, and every
  conversion of a value it makes between the two forms; one made
  without this crate fails the rule

## Forecast

- **`numeric` and `Decimal128`.** A new kind of plan and its
  conversions, both ways; not the plans of other types.
- **A writer of Parquet files, such as `COPY` to a file.** Its callers
  only: the write plans and conversions exist.
- **More of Arrow's types, such as views of strings or nanoseconds.**
  The sets of types and the plans' rules; not the representations.
- **A C API.** Entry points over the plans and conversions, with their
  statuses; not the plans' rules.
- **Explicit vector code for a conversion.** That conversion's loop,
  behind the scalar code of one value as its reference; not the plans'
  rules nor the proofs of that code.
- **A new major version of `arrow-schema`.** The dependency's version
  and the snapshot of the public API; not the plans' rules.
