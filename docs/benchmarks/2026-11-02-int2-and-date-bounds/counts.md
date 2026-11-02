# Counts of the smallint arithmetic, run compare-REgMmG

`tessera-bench --base HEAD --bench arith_int32 --repeats 1` at Tessera
a168afe9, both sides the same build: the program's cases changed with
the change, and the tool compares only cases both sides have. What
counts here is the instructions of a smallint operation against the
int4 one over the same rows and masks. The run passed: 342 cases, none
failed or unstable.

Instructions a row, 1024 rows, every row selected, no NULL; fewer is
better. A dense column holds its values in an array, a Datum column as
Datums.

| column | operation | int4 | smallint | ratio |
|---|---|---:|---:|---:|
| dense | `x + 1` | 3.62 | 4.80 | 1.32 |
| dense | `x + y` | 4.33 | 6.29 | 1.45 |
| dense | `x * 7` | 5.00 | 9.80 | 1.96 |
| dense | `x / 3` | 5.19 | 5.20 | 1.00 |
| Datum | `x + 1` | 4.74 | 6.75 | 1.42 |
| Datum | `x + y` | 6.59 | 8.81 | 1.34 |
| Datum | `x * 7` | 7.43 | 8.23 | 1.11 |
| Datum | `x / 3` | 6.34 | 6.32 | 1.00 |

Over the 19 shapes of each operation (sizes, selections, NULLs), the
median ratio is 1.11 to 1.18 for `+` and `*` and 1.00 for `/`. The
smallint check costs one or two instructions a row where the C code
walked the int4 kernel's output a second time. The dense multiply also
checks the int32 overflow of a product of two smallints, which cannot
happen: the roadmap's "Not placed" keeps it.

The machine code of the int4 and int8 kernels is the same as at
`main` (`tessera-bench --disasm --module kernels`, run disasm-P8itiA):
their loops differ only in the name of the new type parameter, and
the int8 filter only in the offset of a constant.
