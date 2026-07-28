//! Translate the [`Job`](crate::Job)'s language-based stream policy into the
//! library's PID primitive (`libfreemkv::StreamSelection`) for one scanned
//! title. Pure — no I/O. Language identity is resolved with `isolang` so
//! `-a English`, `-a en`, and `-a eng` all match a stream tagged `eng`.

use crate::job::{StreamChoice, StreamFilter};
use isolang::Language;
use libfreemkv::{PidFilter, StreamSelection};

impl StreamChoice {
    /// Translate this choice into the library's PID [`StreamSelection`] for one
    /// scanned title. See [`resolve_stream_selection`].
    pub fn resolve(
        &self,
        title: &libfreemkv::DiscTitle,
    ) -> Result<StreamSelection, StreamSelError> {
        resolve_stream_selection(title, &self.audio, &self.subtitles)
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
    Ok(StreamSelection {
        audio: resolve_class(title, audio, StreamClass::Audio)?,
        subtitle: resolve_class(title, subtitles, StreamClass::Subtitle)?,
    })
}

#[derive(Clone, Copy)]
enum StreamClass {
    Audio,
    Subtitle,
}

fn resolve_class(
    title: &libfreemkv::DiscTitle,
    sel: &StreamFilter,
    class: StreamClass,
) -> Result<PidFilter, StreamSelError> {
    match sel {
        StreamFilter::All => Ok(PidFilter::All),
        StreamFilter::None => Ok(PidFilter::Only(vec![])),
        StreamFilter::Langs(tags) => {
            // Resolve every tag first (surfacing typos before touching streams).
            let wanted: Vec<Language> = tags
                .iter()
                .map(|t| {
                    normalize_lang(t)
                        .ok_or_else(|| StreamSelError::UnknownLanguage { tag: t.clone() })
                })
                .collect::<Result<_, _>>()?;

            // Iterate the class's streams via the lib accessors (cleaner than a
            // Stream-enum match); keep those whose language matches any tag.
            let matches = |lang: &str, pid: u16| -> Option<u16> {
                normalize_lang(lang)
                    .filter(|l| wanted.contains(l))
                    .map(|_| pid)
            };
            let pids: Vec<u16> = match class {
                StreamClass::Audio => title
                    .audio_streams()
                    .filter_map(|a| matches(&a.language, a.pid))
                    .collect(),
                StreamClass::Subtitle => title
                    .subtitle_streams()
                    .filter_map(|s| matches(&s.language, s.pid))
                    .collect(),
            };
            Ok(PidFilter::Only(pids))
        }
    }
}

/// Normalize a language tag (a name, 639-1, 639-2/T, 639-2/B, or 639-3 code) to
/// a language identity, case-insensitively. `None` if unrecognized.
fn normalize_lang(tag: &str) -> Option<Language> {
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
mod tests {
    use super::*;
    use libfreemkv::{
        AudioChannels, AudioStream, Codec, LabelQualifier, SampleRate, Stream, SubtitleStream,
    };

    fn audio(pid: u16, lang: &str) -> Stream {
        Stream::Audio(AudioStream {
            pid,
            codec: Codec::TrueHd,
            channels: AudioChannels::Stereo,
            language: lang.into(),
            sample_rate: SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })
    }
    fn sub(pid: u16, lang: &str) -> Stream {
        Stream::Subtitle(SubtitleStream {
            pid,
            codec: Codec::Pgs,
            language: lang.into(),
            forced: false,
            qualifier: LabelQualifier::None,
            codec_data: None,
        })
    }

    // video(0x1011) + audio eng/spa/fra/eng-commentary + sub eng/fra.
    fn title() -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.streams = vec![
            audio(0x1100, "eng"),
            audio(0x1101, "spa"),
            audio(0x1102, "fra"),
            audio(0x1103, "eng"), // e.g. a commentary track, same language
            sub(0x1200, "eng"),
            sub(0x1201, "fra"),
        ];
        t
    }

    #[test]
    fn all_maps_to_pidfilter_all() {
        let sel =
            resolve_stream_selection(&title(), &StreamFilter::All, &StreamFilter::All).unwrap();
        assert_eq!(sel.audio, PidFilter::All);
        assert_eq!(sel.subtitle, PidFilter::All);
    }

    #[test]
    fn none_maps_to_only_empty() {
        let sel =
            resolve_stream_selection(&title(), &StreamFilter::None, &StreamFilter::None).unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![]));
        assert_eq!(sel.subtitle, PidFilter::Only(vec![]));
    }

    #[test]
    fn full_name_code1_code3_all_match_same_language() {
        for tag in ["English", "english", "en", "eng", "ENG"] {
            let sel = resolve_stream_selection(
                &title(),
                &StreamFilter::Langs(vec![tag.into()]),
                &StreamFilter::All,
            )
            .unwrap();
            // Both eng audio streams (0x1100 and the 0x1103 commentary) match.
            assert_eq!(
                sel.audio,
                PidFilter::Only(vec![0x1100, 0x1103]),
                "tag {tag} should match both eng audio streams"
            );
        }
    }

    #[test]
    fn bibliographic_variant_unifies_with_terminologic() {
        // Request "fre" (639-2/B); stream tagged "fra" (639-2/T) must match.
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["fre".into()]),
            &StreamFilter::All,
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![0x1102]));
    }

    #[test]
    fn multiple_langs_select_union_in_stream_order() {
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["spa".into(), "fra".into()]),
            &StreamFilter::All,
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![0x1101, 0x1102]));
    }

    #[test]
    fn subtitle_langs_are_independent_of_audio() {
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["eng".into()]),
            &StreamFilter::Langs(vec!["fra".into()]),
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![0x1100, 0x1103]));
        assert_eq!(sel.subtitle, PidFilter::Only(vec![0x1201]));
    }

    #[test]
    fn unknown_language_tag_errors() {
        let err = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["Klingonish".into()]),
            &StreamFilter::All,
        )
        .unwrap_err();
        assert_eq!(
            err,
            StreamSelError::UnknownLanguage {
                tag: "Klingonish".into()
            }
        );
    }

    #[test]
    fn lang_missing_on_title_yields_no_pids_for_that_class() {
        // Japanese not on this title: resolves fine (known language), selects
        // nothing here. Preflight decides if that's a disc-wide error.
        let sel = resolve_stream_selection(
            &title(),
            &StreamFilter::Langs(vec!["jpn".into()]),
            &StreamFilter::All,
        )
        .unwrap();
        assert_eq!(sel.audio, PidFilter::Only(vec![]));
    }
}
