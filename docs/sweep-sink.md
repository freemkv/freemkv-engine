# `sweep::SweepSink` — background and rationale

## Why a producer/consumer split

The original sweep loop ran strictly serialised —
SCSI read → decrypt → seek + write → mapfile.record → next iter.
On a healthy disc the SCSI read costs ~5-12 ms per 64 KB batch and
the post-read work (decrypt 1-3 ms + file write + mapfile fsync
5-15 ms) adds another batch's worth of latency. The drive idles
during the post-read work; throughput tops out at the *sum* of
both costs.

A producer/consumer split overlaps the two stages on the generic
[`libfreemkv::io::Pipeline`] + [`libfreemkv::io::Sink`] primitive. This module
is the sweep-specific `Sink` impl; the producer-side state machine
(read_error context, decrypt, set_speed, halt) stays with the producer —
the free `sweep` fn in `recovery/mod.rs`. (It was `Disc::sweep` in
libfreemkv's `disc/mod.rs` before 1.6.0; neither that method nor that
module exists in this crate.)

## Correctness invariants preserved

- Mapfile is single-writer (consumer-only). No locking.
- All `read_error::ReadCtx` state stays on the producer thread.
- `set_speed` calls happen on the producer thread (same thread that
  owns the `SectorSource`). No new SCSI concurrency.
- Per-iteration ordering of file-write → mapfile-record is kept
  intact in the consumer (write before record), so the on-disk
  invariant "mapfile only marks Finished what the file has
  received" survives a crash mid-pass.
- Only one SCSI command is in flight at a time; error-path timing
  is identical and no new retry logic is introduced.

## `SweepSink`

`Sink<WorkItem>` for sweep. Owns the writeback file + mapfile +
progress back-channel. `apply` carries the file-write +
mapfile.record per item; `close` drains the writeback pipeline,
fsyncs the ISO, and flushes the mapfile.

## Test notes

### `a_skip_fill_writes_exactly_the_gap_and_records_exactly_the_gap`

The zero-fill loop must write EXACTLY the skipped range.

`SkipFill`/`GapFill` is the consumer half of damage handling: the
producer could not read `[pos, pos+len)`, so the consumer punches zeros
there and the mapfile records the range `NonTrimmed`. Those two halves
have to agree, and nothing tested them — this module had no tests at
all. If the loop writes nothing, the mapfile still claims the gap was
filled while the image keeps whatever was there before (on a resume,
bytes from a different disc); if it writes past `len`, it clobbers good
data the sweep already wrote just beyond the gap.

`len` here spans more than two 64 KB chunks with a ragged remainder, so
the chunking arithmetic is genuinely exercised rather than short-cut by
a single pass.

### `a_failed_sync_all_is_an_error_when_the_output_is_regular`

`close` must surface a failed `sync_all` when the output is a regular
file.

`output_is_regular` is unit-tested where it is computed; what was
untested is its CONSUMPTION here, which is the half that decides whether
a real durability failure on the just-written image reaches the user or
is thrown away. `sync_all` is the last barrier before `copy` reports a
finished rip — if the swallow ever widened to cover regular files, an
ENOSPC/EIO fsync on the ISO would be reported as a successful rip.

The failure is genuine, not injected: `/dev/null`'s fsync fails at the
OS level (macOS `F_FULLFSYNC` → ENODEV, Linux `fsync` → EINVAL). Only
the `is_regular` flag — the input to the policy under test — is varied
between this test and its pair below, so the two differ in exactly the
bit being asserted.

### `a_failed_sync_all_is_exempt_when_the_output_is_not_regular`

...and must NOT surface it when the output is not a regular file.

`/dev/null` and pipes always fail `sync_all`; treating that as an error
would make every benchmark/sink rip fail at the finish line. The
exemption also has to keep doing the rest of `close`'s job, so this
asserts the mapfile was still flushed and the summary is real — not just
that no error came back.
