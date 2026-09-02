# `src/streams.rs` test rationale

Long-form notes for test-module comments that don't fit the 3-line internal-comment cap.

## `BIB_TERM_ISO1`

This is an INDEPENDENT statement of the standard, not a restatement of
`bib_to_terminologic`. That distinction is the whole point: the previous
test asserted `normalize_lang(bib) == from_639_3(bib_to_terminologic(bib))`,
which routes both sides through the very table under test, so a row could
be wrong and still agree with itself — `"ger" => "fra"` (i.e. `-a ger`
silently selecting the French track) passed the whole suite. The 639-1
column is the oracle: it reaches `isolang` by a path that does not touch
this crate's table at all.

All 20 pairs are listed, so a row DELETED from the production table is
caught as well as a row rewritten.

## `every_bibliographic_code_resolves_to_its_terminologic_language`

Discs label tracks with bibliographic codes (`ger`, `fre`, `chi`) as
often as terminologic ones, so `-a ger` has to reach a German track.
Only `fre` was covered: every other arm could be deleted and the suite
stayed green, which is a track the user asked for silently not matching.

`isolang` is the oracle rather than a restated mapping — the property is
that a bibliographic tag and the /T form the table sends it to name the
SAME language.

## `a_forced_language_the_disc_lacks_is_reported_on_its_own_side`

A forced language the disc does not carry must be REPORTED. The two
subtitle sides are chosen independently, so a hit on the full side says
nothing about the forced one. Before this, asking for forced Japanese on
a disc with none finished at exit 0 as though the request had been
honoured — that half of it silently dropped, which is the whole failure
this check exists to prevent.

## untagged-class-regression

A class whose streams exist but carry NO language tag is a MISS, not an
absent class.

Regression: `class_languages` used to filter empty tags out, so a title
with untagged audio looked identical to a title with no audio — the
unmatched check returned early, nothing was reported, and `resolve`
produced `Only([])`, i.e. keep nothing. `-a eng` on such a disc therefore
wrote a video-only file and exited 0, which is precisely the silent
track-loss the fail-loud work was meant to end. DVDs authored with zero
language bytes reach this (libfreemkv emits `language: ""`).

## `resolve_delegates_to_resolve_stream_selection`

`StreamChoice::resolve` is the method every front-end calls; the free
function underneath it is what every test above calls. That left the
one-line delegation replaceable with `Ok(Default::default())` — an empty
selection, which for a `PidFilter::Only(vec![])` audio choice is a
plausible-looking answer that silently drops every track.

## `german_spanish_audio_german_subs_forced_english`

The request this split exists for, verbatim: "German & Spanish audio,
only German subtitles, and forced only if in English." Audio is a SET —
both German and Spanish are kept, not the first match. Subtitles are a
UNION of two INDEPENDENT sets: the German full subtitle AND the English
forced one, together. Anything that folds the two sides into one language
list can satisfy at most one half of this.
