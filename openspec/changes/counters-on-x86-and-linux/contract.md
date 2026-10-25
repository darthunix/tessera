# pmu: the architecture contract

`pmu` counts four numbers of one thread, in user space: with kperf on
macOS, and with `perf_event_open` on Linux. These rules keep it a
small crate that any program can take as it is, and keep the four
numbers meaning the same wherever it runs. Each rule says what must
not happen and why; how the code is laid out is left to the work.

## Rules

### Free of Tessera
The crate builds without any crate of Tessera and without PostgreSQL,
so that it can be used, and checked, alone.
- **Checked by:** pending

### One meaning in every cell
Each of the four counts is the same event in every cell it counts in:
retired, of the calling thread, in user space; a backend that cannot
count one of them refuses to open rather than count another event in
its place, so that a verdict in any cell rests on the same quantities,
and where a cell sums kinds of cores in a count, as macOS does, the
documentation says so.
- **Checked by:** agent review — list every event each backend
  programs, with its meaning in the vendor's manual and whether it
  counts the kernel; an event that is speculative, that counts the
  kernel, or that stands in for another fails the rule

### Exact or an error
No reading is scaled, estimated, or made from time or from software
events, so that a count is never a guess that looks like a count.
- **Checked by:** agent review — list every path from the result of a
  system call to a `Reading`, with each operation on a count along it;
  any operation but a copy, a difference or a sum fails the rule

### The caller keeps its thread
The crate never changes the CPUs, the priority or the signals of its
caller, and starts no thread, so that what is measured is what the
caller runs.
- **Checked by:** agent review — list every function of the system the
  crate calls; any that changes the calling thread or process, beyond
  opening, setting, reading and closing counters, fails the rule

### A read costs the same each time
A successful read makes one call into the kernel and allocates
nothing, so that its cost inside a measured block is small and fixed.
- **Checked by:** agent review — list every call on the path of a
  successful `read` on each backend; a second call into the kernel or
  an allocation fails the rule

### Unsafe code at the system's edge
Unsafe code lives only in the code that calls the system, and the rest
of the crate forbids it, so that what must be proven stays small.
- **Checked by:** agent review — list every `unsafe` block and every
  item that allows unsafe code; any outside the code that calls kperf,
  `perf_event_open` or the C library fails the rule

### A small surface
The public API grows only with a reason the change gives, so that what
callers rely on stays small, and a published version breaks nothing by
chance.
- **Checked by:** pending

## Forecast

### Reads from user space on Linux
Reading the counters with `rdpmc` touches the Linux backend alone: no
public item, and not the kperf backend.

### A fifth count
Another count, such as misses of the first level of the data cache,
touches `Reading` and its serialized form, which breaks callers, and
the list of events of each backend.

### A backend for another system
A backend for FreeBSD or Windows touches a new module and the choice
of the backend; `ErrorKind` gains a kind only if the new system fails
in a way the kinds do not name.

### Counting over two kinds of cores
Counting a thread that moves between kinds of cores touches the Linux
choice of the core PMU, and `Reading`, which would need a reading for
each kind.
