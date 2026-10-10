//! Translate the [`Job`](crate::Job)'s language-based stream policy into the
//! library's PID primitive (`libfreemkv::StreamSelection`) for one scanned
//! title. Pure — no I/O. Language identity is resolved with `isolang` so
//! `-a English`, `-a en`, and `-a eng` all match a stream tagged `eng`.

use crate::job::{StreamChoice, StreamFilter};
use isolang::Language;
use libfreemkv::{PidFilter, StreamSelection};

// "German subtitles, and forced only if in English" is a single coherent
// request a single language list can't express — forced subs answer to what
// the viewer is *watching* in, not what full subs answer to.
/// A subtitle policy whose FORCED-subtitle language set is independent of its
/// normal-subtitle language set.
///
/// The `forced` side is matched against subtitle streams whose `forced` flag is
/// set (libfreemkv decides forcedness, including its PGS forced probe; this
/// only reads the flag); the `normal` side against the rest. The resolved
/// selection is the UNION of the two: neither side can remove what the other
/// kept, and either may be [`StreamFilter::None`]. A plain [`StreamFilter`]
/// converts into the both-sides-the-same case — see [`From`] below.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubtitleFilter {
    /// Applied to subtitle streams that are NOT flagged forced.
    pub normal: StreamFilter,
    /// Applied to subtitle streams that ARE flagged forced.
    pub forced: StreamFilter,
}

impl SubtitleFilter {
    /// Two independent sides: `normal` subtitles from one policy, forced
    /// subtitles from another. The user's "only German subtitles, and forced
    /// only if in English" is `split(Langs(["de"]), Langs(["en"]))`.
    pub fn split(normal: StreamFilter, forced: StreamFilter) -> Self {
        SubtitleFilter { normal, forced }
    }
}

// One filter applied to BOTH sides: forcedness is ignored, matching what a
// caller with only a single subtitle list means (`Langs(["de"])` keeps every
// German subtitle whether or not it is forced).
impl From<StreamFilter> for SubtitleFilter {
    fn from(f: StreamFilter) -> Self {
        SubtitleFilter::split(f.clone(), f)
    }
}

impl StreamChoice {
    /// Translate this choice into the library's PID [`StreamSelection`] for one
    /// scanned title. See [`resolve_stream_selection`].
    pub fn resolve(
        &self,
        title: &libfreemkv::DiscTitle,
    ) -> Result<StreamSelection, StreamSelError> {
        resolve_stream_selection_forced(title, &self.audio, &self.subtitles)
    }

    /// Report the language-filtered classes that matched NO stream on `title`
    /// while the title HAS streams of that class — i.e. the user asked for a
    /// language that isn't present, so an unguarded rip would ship a file
    /// missing that whole track class with no hint why. See [`UnmatchedClass`].
    ///
    /// Empty result = nothing to warn about: every class was `all`/`none`, or
    /// its language filter matched at least one stream, or the title simply has
    /// no streams of that class (nothing could match).
    pub fn unmatched(&self, title: &libfreemkv::DiscTitle) -> Vec<UnmatchedClass> {
        let mut out = Vec::new();
        let _ = check_class(title, &self.audio, StreamClass::Audio, &mut out);
        // BOTH sides, each against only its own streams: a forced-language miss
        // used to go unreported, exiting 0 as though honoured. The sides are
        // chosen independently, so `available` must not mix them together.
        let _ = check_class(
            title,
            &self.subtitles.normal,
            StreamClass::Subtitle,
            &mut out,
        );
        let _ = check_class(
            title,
            &self.subtitles.forced,
            StreamClass::SubtitleForced,
            &mut out,
        );
        out
    }

    /// The requested language tags no language resolves from, in request order,
    /// deduped: what [`StreamSelError::UnknownLanguage`] would later refuse.
    pub(crate) fn unknown_language_tags(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for sel in [&self.audio, &self.subtitles.normal, &self.subtitles.forced] {
            if let StreamFilter::Langs(tags) = sel {
                for t in tags {
                    if normalize_lang(t).is_none() && !out.contains(&t.as_str()) {
                        out.push(t);
                    }
                }
            }
        }
        out
    }

    /// The class keys whose language request NOT ONE of `titles` can satisfy —
    /// i.e. the whole rip will ship without that track class, however many
    /// titles it writes.
    ///
    /// Unlike [`unmatched`](StreamChoice::unmatched), which answers for ONE
    /// title, a language present on the second title is not a miss just
    /// because the first lacks it. A class NO selected title carries at all is
    /// not reported: there was never a track to keep, so nothing was lost.
    /// Keys are returned in the fixed order audio, subtitle, subtitle_forced.
    pub fn unmatched_everywhere<'a>(
        &self,
        titles: impl IntoIterator<Item = &'a libfreemkv::DiscTitle>,
    ) -> Vec<&'static str> {
        let classes = [
            (StreamClass::Audio, &self.audio),
            (StreamClass::Subtitle, &self.subtitles.normal),
            (StreamClass::SubtitleForced, &self.subtitles.forced),
        ];
        let mut missed = [false; 3];
        let mut matched = [false; 3];
        let mut scratch = Vec::new();
        for title in titles {
            for (i, (class, sel)) in classes.iter().enumerate() {
                scratch.clear();
                match check_class(title, sel, *class, &mut scratch) {
                    ClassVerdict::Missed => missed[i] = true,
                    ClassVerdict::Matched => matched[i] = true,
                    // Not language-filtered, or the title has no such stream:
                    // neither evidence of a miss nor of a hit.
                    ClassVerdict::Unfiltered | ClassVerdict::Absent => {}
                }
            }
        }
        // Before this existed, `-a jpn` on a disc lacking Japanese audio
        // resolved to zero PIDs per-title, muxing a video-only file at exit 0.
        classes
            .iter()
            .enumerate()
            .filter(|(i, _)| missed[*i] && !matched[*i])
            .map(|(_, (class, _))| class.key())
            .collect()
    }
}

/// What one language filter did against one class of one title.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClassVerdict {
    /// `all` / `none` — nothing to miss.
    Unfiltered,
    /// The title carries no stream of this class at all.
    Absent,
    /// At least one stream of the class matched the request.
    Matched,
    /// The class is present and NOTHING matched — the loss this reports.
    Missed,
}

/// A language-filtered class (audio or subtitle) where the requested languages
/// matched no stream on a title that DOES carry that class. The front-end turns
/// this into a hard error (a single-title rip) or a warn-and-skip (a batch), so
/// a typo — or a language simply absent from one title — never silently ships a
/// track-less file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmatchedClass {
    /// Stable class key the front-end localizes: `"audio"`, `"subtitle"` or
    /// `"subtitle_forced"`.
    ///
    /// These are the full set — they are [`StreamClass::key`]'s output, and a
    /// front-end's lookup table must cover all three. `"subtitle_forced"` is
    /// the newest: a forced-subtitle request is filtered independently of the
    /// ordinary subtitle request, so it can go unmatched on its own.
    pub class: &'static str,
    /// The language tags the user requested, verbatim.
    pub requested: Vec<String>,
    /// The languages actually present on the title for this class (sorted,
    /// deduped) — what to show so the user can correct the request.
    pub available: Vec<String>,
}

fn check_class(
    title: &libfreemkv::DiscTitle,
    sel: &StreamFilter,
    class: StreamClass,
    out: &mut Vec<UnmatchedClass>,
) -> ClassVerdict {
    // Only an explicit language filter can "miss"; all/none are always honored.
    let StreamFilter::Langs(tags) = sel else {
        return ClassVerdict::Unfiltered;
    };
    // Keep EVERY stream, including empty-language-tag ones: "no audio" is not a
    // miss, but "audio with no language tag" is (a filter can't match it).
    // Filtering empties out made the two identical, shipping video-only at exit 0.
    let present: Vec<String> = class_languages(title, class);
    if present.is_empty() {
        // The title has no streams of this class at all — nothing to match, and
        // not a "wrong file": there was never a track to keep.
        return ClassVerdict::Absent;
    }
    let wanted: Vec<Language> = tags.iter().filter_map(|t| normalize_lang(t)).collect();
    let any_match = present
        .iter()
        .any(|l| stream_lang(l).is_some_and(|pl| wanted.contains(&pl)));
    if !any_match {
        // Report an untagged track as "und" (ISO 639-2 undetermined), not a
        // blank, so the message reads "available audio: und" instead of
        // trailing off, showing the tracks exist but carry no language.
        let mut available: Vec<String> = present
            .into_iter()
            .map(|l| if l.is_empty() { "und".to_string() } else { l })
            .collect();
        available.sort();
        available.dedup();
        out.push(UnmatchedClass {
            class: class.key(),
            requested: tags.clone(),
            available,
        });
        return ClassVerdict::Missed;
    }
    ClassVerdict::Matched
}

// Raw language tags of every stream of `class` on `title` (order preserved,
// not deduped). Empty tags are RETAINED: an untagged stream still counts as a
// stream of the class, so "no tracks" stays distinguishable from "no tagged".
fn class_languages(title: &libfreemkv::DiscTitle, class: StreamClass) -> Vec<String> {
    match class {
        StreamClass::Audio => title.audio_streams().map(|a| a.language.clone()).collect(),
        StreamClass::Subtitle => title
            .subtitle_streams()
            .filter(|s| !s.forced)
            .map(|s| s.language.clone())
            .collect(),
        StreamClass::SubtitleForced => title
            .subtitle_streams()
            .filter(|s| s.forced)
            .map(|s| s.language.clone())
            .collect(),
    }
}

/// What can go wrong translating a language policy. `Sink`-renderable data, not
/// prose — the front-end localizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamSelError {
    /// A requested tag resolves to no known language — a typo (`"Klingonish"`),
    /// not a disc property. Hard error.
    UnknownLanguage { tag: String },
}

/// The coded form (E9083) for callers that surface a selection error as an I/O error.
impl From<StreamSelError> for libfreemkv::Error {
    fn from(e: StreamSelError) -> Self {
        match e {
            StreamSelError::UnknownLanguage { tag } => {
                libfreemkv::Error::StreamLanguageUnknown { tag }
            }
        }
    }
}

/// Translate the audio + subtitle policy for ONE scanned title into a lib
/// `StreamSelection` (PIDs). A `Langs` tag that resolves to no known language
/// is an [`StreamSelError::UnknownLanguage`]. A resolvable tag that simply has
/// no matching stream on THIS title yields no PIDs for it here — the caller
/// (preflight) decides whether that is a disc-wide error or a per-title skip.
pub fn resolve_stream_selection(
    title: &libfreemkv::DiscTitle,
    audio: &StreamFilter,
    subtitles: &StreamFilter,
) -> Result<StreamSelection, StreamSelError> {
    resolve_stream_selection_forced(title, audio, &subtitles.clone().into())
}

/// As [`resolve_stream_selection`], but the subtitle policy carries an
/// independent forced-subtitle language set ([`SubtitleFilter`]). The subtitle
/// PIDs are the UNION of (non-forced streams matching `subtitles.normal`) and
/// (forced streams matching `subtitles.forced`), in stream order.
///
/// `resolve_stream_selection` is this function with the same filter on both
/// sides, so there is one matcher, not two.
pub fn resolve_stream_selection_forced(
    title: &libfreemkv::DiscTitle,
    audio: &StreamFilter,
    subtitles: &SubtitleFilter,
) -> Result<StreamSelection, StreamSelError> {
    Ok(StreamSelection {
        audio: resolve_audio(title, audio)?,
        subtitle: resolve_subtitles(title, subtitles)?,
    })
}

#[derive(Clone, Copy)]
enum StreamClass {
    Audio,
    Subtitle,
    /// Forced subtitles, checked SEPARATELY from full ones. The two are chosen
    /// independently ("German subtitles, forced only if English"), so a request
    /// that matches nothing on one side says nothing about the other.
    SubtitleForced,
}

impl StreamClass {
    /// Stable, localizable key for this class.
    fn key(self) -> &'static str {
        match self {
            StreamClass::Audio => "audio",
            StreamClass::Subtitle => "subtitle",
            StreamClass::SubtitleForced => "subtitle_forced",
        }
    }
}

// A StreamFilter with its language tags already resolved to identities: every
// tag is validated ONCE, before any stream is looked at, so a typo is reported
// as UnknownLanguage whether or not the disc has a stream that would match.
enum Wanted {
    All,
    None,
    Langs(Vec<Language>),
}

pub(crate) struct AudioPreference(Vec<Language>);

impl AudioPreference {
    pub(crate) fn new(audio: &StreamFilter) -> Self {
        let Ok(Wanted::Langs(mut wanted)) = Wanted::compile(audio) else {
            return Self(Vec::new());
        };
        let mut seen = std::collections::HashSet::new();
        wanted.retain(|language| seen.insert(*language));
        Self(wanted)
    }

    // Count requested languages, not tracks: duplicate tracks add no coverage.
    pub(crate) fn score_languages(&self, languages: &[String]) -> usize {
        self.0
            .iter()
            .filter(|&&language| {
                languages
                    .iter()
                    .any(|tag| stream_lang(tag) == Some(language))
            })
            .count()
    }
}

impl Wanted {
    fn compile(sel: &StreamFilter) -> Result<Wanted, StreamSelError> {
        Ok(match sel {
            StreamFilter::All => Wanted::All,
            StreamFilter::None => Wanted::None,
            StreamFilter::Langs(tags) => Wanted::Langs(
                tags.iter()
                    .map(|t| {
                        normalize_lang(t)
                            .ok_or_else(|| StreamSelError::UnknownLanguage { tag: t.clone() })
                    })
                    .collect::<Result<_, _>>()?,
            ),
        })
    }

    /// Does this side keep a stream tagged `lang`? An untagged stream is `und`; an
    /// unresolvable tag can only be kept by `All`.
    fn keeps(&self, lang: &str) -> bool {
        match self {
            Wanted::All => true,
            Wanted::None => false,
            Wanted::Langs(langs) => stream_lang(lang).is_some_and(|l| langs.contains(&l)),
        }
    }
}

fn resolve_audio(
    title: &libfreemkv::DiscTitle,
    sel: &StreamFilter,
) -> Result<PidFilter, StreamSelError> {
    // `All` stays the library's `All` rather than an enumerated PID list, so an
    // unfiltered rip is byte-identical to no selection at all.
    if matches!(sel, StreamFilter::All) {
        return Ok(PidFilter::All);
    }
    let wanted = Wanted::compile(sel)?;
    // A SET, not a fallback chain: every audio stream matching ANY listed
    // language is kept, so "German & Spanish audio" keeps both.
    Ok(PidFilter::Only(
        title
            .audio_streams()
            .filter(|a| wanted.keeps(&a.language))
            .map(|a| a.pid)
            .collect(),
    ))
}

// Subtitle PIDs = (non-forced streams matching sel.normal) ∪ (forced streams
// matching sel.forced). Each stream is classified by the `forced` flag
// libfreemkv already set (never re-derived), so the two sides never contend.
fn resolve_subtitles(
    title: &libfreemkv::DiscTitle,
    sel: &SubtitleFilter,
) -> Result<PidFilter, StreamSelError> {
    // Both sides unfiltered is the archival default: keep the library's `All`.
    if matches!(sel.normal, StreamFilter::All) && matches!(sel.forced, StreamFilter::All) {
        return Ok(PidFilter::All);
    }
    // Compile both sides up front: a typo on the forced side is an error even
    // when the disc carries no forced subtitle at all.
    let normal = Wanted::compile(&sel.normal)?;
    let forced = Wanted::compile(&sel.forced)?;
    Ok(PidFilter::Only(
        title
            .subtitle_streams()
            .filter(|s| {
                if s.forced {
                    forced.keeps(&s.language)
                } else {
                    normal.keeps(&s.language)
                }
            })
            .map(|s| s.pid)
            .collect(),
    ))
}

/// Normalize a language tag (a name, 639-1, 639-2/T, 639-2/B, or 639-3 code) to
/// a language identity, case-insensitively. `None` if unrecognized.
// A stream's language: an untagged one is `und` (undetermined), as the refusal reports it.
fn stream_lang(tag: &str) -> Option<Language> {
    match tag.trim().is_empty() {
        true => Some(Language::Und),
        false => normalize_lang(tag),
    }
}

pub(crate) fn normalize_lang(tag: &str) -> Option<Language> {
    let t = tag.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // 639-1 two-letter, then 639-3/639-2-T three-letter.
    Language::from_639_1(&lower)
        .or_else(|| Language::from_639_3(&lower))
        // 639-2/B bibliographic (fre, ger, dut, …) → 639-3/T, then resolve.
        .or_else(|| bib_to_terminologic(&lower).and_then(Language::from_639_3))
        // Full English name, case-insensitive.
        .or_else(|| Language::from_name_lowercase(&lower))
}

/// The ISO 639-2/B (bibliographic) codes that differ from 639-2/T (=639-3),
/// mapped to their /T form so `isolang::from_639_3` can resolve them.
fn bib_to_terminologic(code: &str) -> Option<&'static str> {
    Some(match code {
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "cze" => "ces",
        "chi" => "zho",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "mao" => "mri",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        _ => return None,
    })
}

#[cfg(test)]
#[path = "streams_tests.rs"]
mod tests;
