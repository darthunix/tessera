## Context

`compare` in `tessera-kernels` is generic over two `Strings` sources and
is made once for each pair of sources the C entry point meets: column
and column, column and scalar, scalar and column. For bpchar it compared
`a[..bpchar_len(a)]` with `b[..bpchar_len(b)]`, both trimmed per row.

## Goals / Non-Goals

**Goals:** the same rows kept and set aside, with the scalar's spaces
counted once a call and a value's padding read only when its first
bytes match.

**Non-Goals:** text (not bpchar) equality, already a slice comparison
that checks lengths first; a column against a column; LIKE.

## Decisions

### The scalar asks itself

`Strings::constant()` defaults to `None`, so a column keeps its code;
the branch is decided when `compare` is made for a pair of sources. The
trait says what `compare` relies on: when `constant()` gives bytes,
`get` gives the same bytes for every row.

### First bytes, then spaces

For a trimmed scalar `t`, a value equals it when it is at least as long,
its first `|t|` bytes are `t`, and the rest are spaces: `t` is empty or
ends in a non-space, so this is exactly `trim(value) == t`. Most values
differ in their first byte and are decided there.

## Risks / Trade-offs

- A space byte inside a multibyte character: none in a server encoding
  PostgreSQL allows, which `bcTruelen` relies on as well.

## Measurement

`cargo tpch`, base and change twice in turn, serial: Q19, Q12, Q10, and
Q1 and Q6 as controls. `tessera-bench --disasm --module kernels` to see
that only `tess_text_compare` changes.
