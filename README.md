![Rust](https://github.com/ryzhyk/malloc_freq/workflows/Rust/badge.svg)

# Malloc frequency profiler

This malloc frequency profiler helps detect program hotspots that perform a large number of
memory allocations.  Many small allocations can slow down your application
and lead to memory fragmentation.  Typical symptoms include seeing `malloc` at the top of the
application's CPU profile, as well as observing that the resident set size (RSS) of the
appliciation significantly exceeds the amount of memory it allocates from the heap.

Note: `malloc_freq` is different from heap profilers like `valgrind`'s `massif`, which track
program's heap usage at any give point in time, in two ways.  First, heap profilers ignore
short-lived allocations that do not affect the heap size significantly.  Second, heap profilers
track the amount of allocated memory, whereas `malloc_freq` focuses on the number of calls
to `malloc`.

`malloc_freq` can be used to profile programs in Rust or any other compiled language.

## Enabling `malloc_freq` in a Rust program

To enable `malloc_freq` in a Rust program, configure it as a global allocator instead of
[`std::alloc::System`]:

```
use malloc_freq::ProfAllocator;

#[global_allocator]
static GLOBAL: ProfAllocator = ProfAllocator;

fn main() {}
```

## Using `malloc_freq` via `LD_PRELOAD`

For programs in other languages, including a mix of Rust, C, etc., use the companion
[`lib_malloc_freq`] crate, which produces a dynamic library that can be used via `LD_PRELOAD`
to intercept all `malloc` calls issued by the program:

```bash
LD_PRELOAD=libmalloc_freq.so ./my_program
```

## Viewing `malloc_freq` profiles

Upon **successful** program termination, `malloc_freq` stores the malloc profile in
the `malloc_freq.<pid>` directory.  The directory will contain a file for each thread in the
program.  To view the profile, use the `mf_print` tool from this crate, e.g.:

```bash
mf_print --dir malloc_freq.<pid> --threshold 0.2
```

where the `--threshold` switch specifies the significance threshold of profile entries in
percents (profile entries with fewer than `threshold`% `malloc` calls are skipped in the output).
`mf_print` outputs a human-readable profile that lists program callstacks that perform `malloc`
calls, annotated with the number of `malloc` calls and the total number of bytes allocated by each
stack trace.

## TODOs

- Generate profile on abnormal program termination (e.g., panic or `SIGKILL`).

- Subsampling for lower overhead.  `malloc_freq` can slow down the target
  program significantly, as it currently intercepts all `malloc` calls and
  records their stack traces int he profile.
