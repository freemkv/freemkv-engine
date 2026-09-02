# `job` module design notes

## Why `SubtitleFilter` is imported from `crate::streams` instead of defined here

"Pure data" is about I/O, not about module position: the request shape reaches
into [`crate::streams`] for `SubtitleFilter`, which is itself plain data. It
lives there because that is where the filter is APPLIED and where its
language-matching rules are documented, and duplicating it here to keep the
import graph one-directional would give the two copies a way to disagree
about what a filter means.

## Why `subtitles` is one `SubtitleFilter` field, not two `StreamFilter` fields

"German subtitles, forced only if English" is one coherent request that a
single language list cannot express — the forced side is a different
editorial object. A caller that only has one list assigns it directly
(`subtitles: my_filter.into()`); the `From` impl applies it to BOTH sides,
which is exactly the pre-forced meaning. It is one field, not two, so the two
sides cannot drift out of sync or be partially updated.
