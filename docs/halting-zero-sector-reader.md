# `HaltingZeroSectorReader`

Zero-filling reader that raises the halt flag itself on its Nth read and
counts every read the sweep issues, so a halt test can assert on a count
instead of a stopwatch.

The interesting number is `reads_total - halt_after_reads`: how many more
batches the sweep read after the flag went up. The sweep polls `halt` at the
top of every batch iteration, immediately before the read, so a correct
build issues exactly ZERO further reads — no wall-clock budget involved, and
no dependence on how fast the machine happens to be.
