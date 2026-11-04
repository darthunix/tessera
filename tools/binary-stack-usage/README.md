# binary-stack-usage

Prints the stack frame of each function in a built library, largest
first. A stack frame is the memory one call of a function takes on the
stack. The script reads the disassembly of the finished library, so its
numbers hold after inlining, for C and for Rust alike.

```sh
python3 tools/binary-stack-usage/binary-stack-usage.py --limit 30 \
    kernels/tessera_kernels.so
```

The library ends in `.dylib` on macOS. Frames under 1 KB are left out of
the list; `--min 0` prints them, and `--help` names the other options.
The script needs Python 3.10 or later and `objdump`, the GNU one or
`llvm-objdump`. It reads x86_64 and aarch64, in ELF and Mach-O.

Read a release build. A debug build inlines little, so its frames say
little about the library that runs.

## Reading the numbers

The first column is the frame in bytes.

- PostgreSQL checks the depth of the stack only where
  `check_stack_depth` or `stack_is_too_deep` is called, and it asks that
  `max_stack_depth` stay 512 KB (`STACK_DEPTH_SLOP`) under the limit of
  the system. That margin is the space the calls below the last check
  can count on, all their frames together, so read a large frame with
  the chain of calls it sits in.
- A large frame costs little by itself, since the stack pointer only
  moves. On a hot path it can still cost a lot: the frame may be zeroed
  at each call, and on macOS a large frame calls `___chkstk_darwin`,
  which touches its pages. A large frame on a hot path is a reason to
  read the disassembly.

## What it does not show

- The depth of a chain of calls: each frame is counted alone.
- Recursion: a small frame can still take the whole stack.
- Dynamic allocations, `alloca` and variable length arrays in C.
- The red zone on x86_64: a function that calls nothing may use up to
  128 bytes below the stack pointer, and they are not counted.
- Every name of a function: objdump shows one name for the symbols at
  one address, and `--filter` sees only that one.

## Tests

```sh
python3 -m pytest tools/binary-stack-usage
```

The tests assemble `cases/` for each target and compare the frames with
the sizes in the names of the functions. They run under pytest and need
`clang` and `llvm-objdump`, and on an x86_64 host GNU `objdump` too. A
test whose tool is missing is skipped, except in CI, where it fails. On
macOS, `llvm-objdump` comes from Homebrew's `llvm`, which is not on
`PATH` by default: `PATH="$(brew --prefix llvm)/bin:$PATH"`.

## Where it comes from

The script and `cases/` are copies of `tools/binary-stack-usage.py` and
`test/tools/binary_stack_usage/` from
[Picodata](https://git.picodata.io/core/picodata), commit `11a4e1131`.
They are unchanged, and the script stays in Python, so that an update
is a copy over them. The test differs from the original in its paths
and in how it finds out that it runs in CI.

These files keep Picodata's BSD license, in [LICENSE](LICENSE); the
AUTHORS file it names is in that repository.
