# `tests/recovery_copy_dispatch.rs` — background notes

Long-form rationale for individual regression tests in
`tests/recovery_copy_dispatch.rs`, relocated here to keep the doc comments in
the test file within the internal-comment cap. Each section is pointed to by
a one-line `// See docs/recovery-copy-dispatch.md — <topic>` comment above
the relevant item.

## unique_title — why the suffix needs to be unique per run

`Disc::mapfile_for(Path::new("/dev/null"))` has nowhere to put a sibling
mapfile, so it derives `$TMPDIR/<sanitized-title>.mapfile` — a fixed path
with no pid and no counter. `CleanupGuard` removes it on a normal exit or an
unwind, but not after a SIGKILL, a test-binary timeout, or a hard abort, so
a killed run leaves a mapfile behind that the NEXT run resumes from instead
of sweeping. That makes `sweep_dev_null_full_good`'s
`bytes_good == sectors * 2048` fail for reasons that have nothing to do with
the code under test (the three `/dev/null` tests already use distinct titles
— T2/T3/T5 — so intra-run collision was never the problem; cross-run residue
is).

pid + nanosecond timestamp + a per-process counter makes the derived path
unique per run, so a stale file can never be mistaken for this run's. Only
`[A-Za-z0-9-_]` survives `mapfile_for`'s sanitizer, so the suffix uses
digits and `-` only and reaches the filename intact.

## sweep_marks_bad_region_nontrimmed_and_engages_damage_jump

End-to-end Pass-1 sweep against a synthetic `MockReader` with an injected
bad-sector region, asserting the RESULTING MAPFILE — the thing the sweep
loop and damage-jump exist to produce. Drives the real `sweep` (no
live drive, per the project's "synthetic fixtures only" rule) and checks:
  * the leading good region is marked Finished,
  * the bad region (and the skip-ahead gap the damage-jump zero-fills) is
    marked NonTrimmed,
  * the damage-jump actually engaged — the NonTrimmed span is far larger
    than the single failed ECC batch, which only happens if Pass-1 jumped
    ahead (JUMP_BASE_SECTORS×batch) and zero-filled the gap as NonTrimmed,
  * the mapfile covers the whole disc with no overlap, and good+retryable
    accounting matches.

Note: this exercises the real cooldown/pause pacing, so it spends a few
seconds of wall time on the single zone-entry pause (same cost the
existing `sweep_to_dev_null_real` already pays) — but unlike that test it
asserts the actual recovery bookkeeping, not just `is_ok()`.

## sweep_resume_downgrades_on_zero_iso_with_progress_mapfile

Regression (resume/mapfile consistency, MED): a resume sweep against a
mapfile that claims prior progress (Finished ranges) while the ISO is
missing/zero-length must DOWNGRADE to a fresh full sweep — NOT reuse the
stale mapfile. The producer only builds work from NonTried ranges, so a
reused mapfile would leave every Finished range unread and ZERO in the
new ISO (a silent hole). Reachable via autorip ResumeMode::Require when
the ISO was deleted/truncated but the mapfile survived. The fresh-sweep
downgrade self-heals: all ranges are re-read and the ISO is fully
populated.

## sweep_resume_downgrades_on_corrupt_mapfile

Regression (resume reconciliation, MED follow-on): a resume sweep against
a CORRUPT / unparseable mapfile must DOWNGRADE to a fresh full sweep —
not proceed with resume=true (which would hand a garbage/empty mapfile to
open_or_create and silently skip ranges). The `load()` Err arm sets
resume=false; the `!resume` path then drops the corrupt mapfile and the
rip restarts clean. Consistent with the total_size-mismatch downgrade.

## sweep_fresh_aborts_when_stale_mapfile_unremovable

Regression: a fresh (non-resume) sweep MUST abort if the stale mapfile
cannot be removed, rather than swallowing the error and letting
`open_or_create` load the stale file (which would make the new disc
inherit old Finished ranges → silently zero-filled ISO). We force the
remove to fail with a non-ENOENT error by placing a NON-EMPTY DIRECTORY
at the mapfile path (`remove_file` on a dir fails, and a non-empty dir
can't be ENOENT).

## resume_sweeps_nontried_tail_even_with_retryable_present

Finding #6 regression: on resume, copy() must NOT abandon the un-swept
NonTried tail when retryable (NonTrimmed) bytes also remain. The mapfile
covers the disc and has BOTH a NonTrimmed (retryable) range and a
NonTried tail; dispatch must route to a resume sweep first so the tail is
actually read. Before the fix, `bytes_retryable > 0` short-circuited to
patch and the NonTried tail was silently left unread.

## plain_copy_resumes_nontried_tail_after_interrupt

Regression (rc.6 user fix): a PLAIN (non-`--multipass`) `disc:// → iso://`
copy interrupted by Ctrl-C must RESUME from where it stopped when the
SAME command is re-issued — not restart from sector 0. The CLI help and
`rip_iso` examples promise "auto-resumes if interrupted". Before the fix
the whole mapfile-resume dispatch in `copy` was gated behind
`if opts.multipass`, so a plain copy always called
`sweep_internal(resume=false)`, which wiped the mapfile + ISO and swept
the disc again from LBA 0.

Simulate an interrupted plain sweep: a mapfile that covers the disc with
a Finished prefix [0..100) and a NonTried tail [100..200). A plain re-run
must read ONLY the tail (resume) and leave the prefix untouched.

## sweep_to_dev_null_recovers_via_patch

A patch pass whose destination is the `/dev/null` sink resumes from the
mapfile and recovers what the sweep could not.

This test used to sweep to a tempdir ISO and then "patch" `/dev/null`, which
patched nothing: `Disc::mapfile_for` deliberately redirects a `/dev/null`
destination to `$TMPDIR/<title>.mapfile`, so the second call never saw the
mapfile the first one wrote — it was a fresh full sweep of a clean reader
wearing a patch pass's name. Deleting the entire sweep half left it green,
and its only assertion was `is_ok()`, which a `copy` that read nothing also
satisfies. It also had no `CleanupGuard`, so it leaked that fixed temp path
on every run AND could resume a previous run's leftovers.

Both calls now target `/dev/null`, so they share one mapfile and the second
really is a patch pass over the first's damage.

## sweep_pipeline_full_good_100_batches

Synthetic regression test for the 0.18 SweepSink + Pipeline migration.
~100 batches of clean reads (6000 sectors at the default 60-sector
single-pass batch size); verifies all bytes land in the ISO and the
consumer's final stats match the input. The throughput regression check
(vs 0.17.13) is a separate manual / live-drive concern; here we only
assert correctness.

## complete_mapfile_with_a_missing_iso_re_reads_instead_of_claiming_success

`copy()`'s "already complete, don't re-read a finished ISO" shortcut must
verify the ISO is still THERE.

The shortcut trusts the mapfile alone: identity matches, every range is
Finished, so it returns success without reading a sector. But the mapfile
and the ISO are two files, and only one of them is being checked. Delete or
truncate the ISO — a staging cleanup, a remount, an operator freeing space —
and the mapfile still says "complete", so the call reports bytes_good = the
whole disc with no image on disk at all. The caller then muxes from
nothing.

`sweep()` already guards this exact case (see
`sweep_resume_downgrades_on_zero_iso_with_progress_mapfile`); the dispatch
shortcut in `copy()` simply never got the same check. The rip must
self-heal into a fresh sweep instead of claiming a success it cannot back.

## resume_against_a_truncated_iso_re_reads_instead_of_leaving_a_hole

A resume against a SHORT ISO must re-read, not leave a hole.

The inconsistent-resume guard's own comment says "missing or truncated",
but it only ever tested for zero length. An ISO truncated to a non-zero
length — a partial copy, a full disk, an interrupted transfer — therefore
passed the guard and resumed. Since the producer builds work only from
NonTried ranges, every Finished range beyond the truncation point is never
re-read, so it stays a hole in an image the mapfile calls complete.

## encrypted_disc_with_no_cipher_state_is_refused_not_written_as_ciphertext

A disc that reports itself encrypted but resolved NO cipher state at all
(`aacs: None`, `css: None` — scan sets `encrypted` from the presence of
/AACS, and leaves `aacs: None` with `aacs_error: Some(..)` when the VID
probe fails) must be REFUSED, not written out as ciphertext.

`ensure_decryptable` only errors when it has an aacs/css state to judge, so
this disc slipped through, the decrypt wrapper became a pass-through, and
the copy finished at `complete: true`, exit 0 — with an unplayable
ciphertext ISO on disk. Preflight blocks this disc; the executor didn't.

## mapfile_from_a_different_disc_is_refused

A mapfile left by a DIFFERENT disc of the same capacity must not be
resumed. Two box-set reprints authored at the same size satisfy the old
`total_size == capacity_bytes` gate, so disc A's Finished ranges were
trusted for disc B — never re-read — and the ISO silently spliced sectors
from two physical discs while passing every completeness check.

## a_plain_copy_aborts_on_the_first_bad_sector_instead_of_holing_the_iso

A PLAIN copy (no `--multipass`) must ABORT on the first unreadable sector,
not zero-fill and carry on.

`Err(err) if !opts.skip_on_error => { producer_err = ...; break 'outer }` is
the whole behaviour of `disc:// -> iso://` without `--multipass`, because
`sweep_internal` sets `skip_on_error: opts.multipass`. The mutation run
forced that guard to `false` and the suite stayed green: the error then
falls into the recovery arm, so the sweep zero-fills the bad region, marks
it NonTrimmed, damage-jumps and returns Ok — a plain copy of a damaged disc
exits 0 with a holed ISO.

It survived because every `CopyOptions` in this file sets `multipass: true`
and every direct `SweepOptions` sets `skip_on_error: true`, so nothing ever
ran a sweep with `skip_on_error: false` over a bad sector.

## a_resume_does_not_truncate_the_already_recovered_prefix

A RESUME must not truncate the image it is resuming into.

`if resume && existing_len.is_some_and(|len| len > 0)` chooses open-existing
over `File::create` + `set_len` — i.e. over truncation. The mutation run
changed `>` to `<` and to `==`; both send every resume down the
create-and-truncate branch, zeroing bytes the mapfile still records as
Finished. The producer only builds work from NonTried ranges, so those
bytes are never re-read: silent, total loss of the recovered image.

The existing resume tests could not catch it because they pre-fill the ISO
with ZEROS and assert only which LBAs were read — truncating zeros to zeros
is invisible. This one fills the recovered prefix with a recognisable
pattern instead.

## a_fresh_sweep_truncates_the_image_left_by_a_previous_run

A FRESH sweep must truncate an image left over from a previous run.

The counterpart to the resume test above, and the other half of the same
condition: `resume && existing_len > 0`. Mutated to `||`, a fresh sweep
over a pre-existing image OPENS it instead of creating it — so wherever the
new sweep does not reach, the previous disc's bytes survive underneath and
are handed to the muxer as this disc's data.

The sweep is halted partway so there IS a region the new sweep never
reaches; a fresh create + `set_len` leaves that region zeroed.

## a_resume_into_an_empty_image_pre_sizes_it_to_the_disc

A resume into a zero-length image must still pre-size it.

A mapfile that is entirely NonTried claims no progress, so the
inconsistent-resume guard leaves `resume = true` even with a zero-length
ISO on disk. `existing_len > 0` is then what routes it to the create +
`set_len` branch; widened to `>=`, `Some(0)` takes the open-existing branch
and the pre-size never happens. A halt then leaves the image shorter than
the disc — and on the NEXT run the inconsistent-resume guard sees a short
ISO against a mapfile that now does claim progress, throws the whole
mapfile away, and re-rips from LBA 0.

## a_resume_whose_image_was_deleted_starts_over_instead_of_erroring

A resume whose image was DELETED must self-heal into a fresh sweep.

The inconsistent-resume guard reads the image length, and `NotFound` is the
one error that means "no file yet" — anything else aborts. Classify a
missing file as an unknown error and a resume whose ISO was cleaned up
returns an error instead of simply starting over.

The existing downgrade test writes a zero-LENGTH file, which exists; this
one removes it.

## the_mapfile_header_carries_the_unit_keys_when_there_are_keys

KEYS XOR VID: the mapfile header carries one or the other, never both.

A keyed disc writes its unit keys — the final answer, so deferred mux
decrypts directly with no key-service round trip. An unresolved disc writes
only the VID, the retry marker. Deleting the `!` swaps them: a keyed disc
records the VID and calls `set_unit_keys(&[])`, and deferred mux loses the
keys it was promised. `mapfile.rs` tests the setters; nothing tested the
wiring in `sweep`.

## the_drive_retry_lever_is_the_inverse_of_skip_on_error

The drive's in-drive retry lever is the INVERSE of skip-on-error.

Pass 1 of a multipass rip is "fast and accurate — get the most data in the
shortest time", so it asks the drive for fast-fail reads and handles damage
itself; a plain copy, which aborts on the first error, asks the drive to
retry hard before giving up. Invert `let recovery = !opts.skip_on_error`
and both modes get the wrong lever — a multipass Pass 1 crawls through
in-drive retries on every batch of a damaged disc. Invisible in the ISO and
the mapfile: it is a flag on a SCSI read.

## damage_drops_the_drive_speed_and_a_clean_run_restores_it

Damage slows the drive down, and a clean run brings it back up.

Entering a damage zone drops the drive to its minimum read speed, and
sixteen consecutive good batches restore maximum. Delete the `!` on the
entry check and the zone is never entered at all (the flag starts false),
so the drive is never slowed on damaged media and the whole recovery
behaviour quietly disappears — no test noticed, because a mock reader
ignores `set_speed`.

## re_issuing_a_finished_copy_reads_nothing_and_rewrites_nothing

A finished rip with an intact image is a no-op — no reads, no re-write.

The dispatch shortcut is `covers_disc && bad_bytes == 0 && nontried == 0 &&
!iso_is_intact` for the repair path, and the same conjunction with
`iso_is_intact` for "already done". Loosen either and a COMPLETE rip with a
perfectly good ISO takes `sweep_internal(resume = false)` — which removes
the mapfile, `File::create`s the image and re-rips the whole disc, undoing
a finished job.

## a_disc_whose_damage_is_all_permanent_is_not_patched_again

An all-Unreadable disc is terminal — it must not be patched again.

Once every bad sector has been promoted to Unreadable there is nothing
retryable left, and the documented fallthrough returns the terminal result
immediately. `bytes_retryable > 0` mutated to `>= 0` is always true for a
u64, so that fallthrough never runs and every finished-but-lossy disc gets
one more patch pass — and `patch` selects `damage_sector_statuses()`, which
INCLUDES Unreadable, so it re-reads ranges the design considers permanently
lost.

## equal_pending_and_unreadable_counts_still_route_to_a_patch_pass

Retryable bytes are not "no bad bytes", even when they exactly cancel.

`bad_bytes = bytes_pending + bytes_unreadable`, and the two are DISJOINT
counters — Unreadable feeds only `bytes_unreadable`, while NonTried /
NonTrimmed / NonScraped feed `bytes_pending`. Turn the `+` into a `-` and
the two cancel whenever they happen to be equal, so `bad_bytes == 0` and
the dispatch takes the "already complete" shortcut instead of routing to
`patch`: retryable bytes silently abandoned and the rip reported terminal.

## a_fresh_sweep_over_a_different_discs_mapfile_starts_over

A fresh sweep over another disc's mapfile drops it and sweeps.

`if resume && mapfile_path.exists()` guards the identity check. Widened to
`||`, a FRESH sweep runs the identity check against the leftover mapfile
and errors out — a new disc in the drive after a previous rip refuses to
start, instead of doing the obvious thing and starting over. The existing
identity test only covers the resume path, which is the direction that must
error.

## a_decrypting_css_sweep_descrambles_the_scrambled_sectors

A DECRYPTING sweep must actually decrypt — and the crate never once ran one.

Every `CopyOptions`/`SweepOptions` in this suite sets `decrypt: false`, and
the two AACS fixtures are refused pre-flight, so the whole decrypt-wiring
triangle at the top of `sweep` — resolve a whole-disc AACS key map, or fall
back to the CSS self-descramble path — was unexercised. Widening
`opts.decrypt && decrypt_is_aacs` to `||` installs an AACS key map on a CSS
disc, and a `Some(key_map)` takes the mapped early-return in
`DecryptingSectorSource` and never reaches the CSS descramble at all: a
`--decrypt` CSS rip writes scrambled bytes to the ISO and exits 0. That is
exactly the silent-garbage-success this file's pre-flight gate exists to
stop, arriving one layer below the gate.

A CSS sector carries its own scramble flag in bits 4-5 of byte 0x14, so the
fixture can mark some sectors scrambled and leave others clear and the
assertion is simply: the scrambled ones changed, the clear ones did not.

## a_short_read_is_a_failed_read_and_never_reaches_the_image

A reader that under-delivers must not have its buffer written to the ISO.

The sweep's producer reads into ONE `buf` that is reused for every block,
then ships `buf[..block_bytes]` down the pipeline as `WorkItem::Good`. The
match arm read `Ok(_)`, discarding the byte count `SectorSource` returns —
so a source that answered `Ok(n)` with `n < requested` would have the tail
of the PREVIOUS block written into the image and recorded `Finished`. A rip
that exits 0 with a perfect mapfile and somebody else's sectors inside the
movie is the most expensive failure this crate can produce, and no reader in
the tree does this TODAY — which is exactly why the guard has to be here
rather than in the readers.

The fixture makes the corruption visible: block 0 comes back in full as
0xBB, then every later read claims only ONE sector (0x11) of the eight it
was asked for. Revert `require_full_read` at the sweep site and this test
fails twice over — `sweep` returns `Ok`, and the image carries 0xBB in
blocks the drive never delivered.
