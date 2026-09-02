# Forced-subtitle selection: the request behind this test file

A real user asked for this exact selection: "German & Spanish audio, only
German subtitles, and forced only if in English." Three things have to hold
at once for that sentence to be expressible, and each has failed in some
earlier design:

  * audio is a SET (German AND Spanish both kept), not a first-match chain;
  * the forced language set is INDEPENDENT of the non-forced one, so
    "German subtitles + forced English" is not a contradiction;
  * a plain single list still means what it always did, so `-a eng` and
    `-a none` are unaffected.

The unit tests in `streams.rs` cover the matcher. This file covers the part
a front-end actually touches: that the types are nameable, constructible and
wired to the same behaviour from outside the crate.
