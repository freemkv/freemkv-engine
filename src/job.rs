//! What to rip, and how — the front-end's request to the engine.
//!
//! A [`Job`] is pure data: a front-end builds one from CLI args, a web POST, or
//! GUI selections, hands it to [`crate::preflight`] to check it, then to
//! [`crate::run`] to execute it. It carries no I/O handles and no callbacks —
//! those arrive separately as the [`crate::Sink`].
//!
//! "Pure data" is about I/O, not about module position: the request shape
//! reaches into [`crate::streams`] for [`SubtitleFilter`], which is itself
//! plain data. It lives there because that is where the filter is APPLIED and
//! where its language-matching rules are documented, and duplicating it here
//! to keep the import graph one-directional would give the two copies a way to
//! disagree about what a filter means.

use crate::streams::SubtitleFilter;

/// Which recovery strategy the rip uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RipMode {
    /// One pass, disc→MKV, no retries. Fast; accepts whatever the first read
    /// returns. (Maps to autorip/CLI `rip_mode = "single"`.)
    #[default]
    Single,
    /// Sweep + targeted patch passes over an ISO intermediate, with an
    /// abort-on-loss check after retries are exhausted. (`rip_mode = "multi"`.)
    Multi,
}

/// Which titles to rip. Mirrors the desktop-UI "Select: [Main movie ▾]"
/// control and the CLI's title-filter flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Selection {
    /// The main feature only (canonical title index 0). The common case.
    #[default]
    MainMovie,
    /// Every title on the disc.
    All,
    /// The single longest title (may differ from the canonical main feature on
    /// odd authoring).
    Longest,
    /// An explicit set of canonical title indices.
    Titles(Vec<usize>),
}

/// A rip request. Front-ends construct this; the engine consumes it.
#[derive(Clone, Debug)]
pub struct Job {
    /// Source URL/path: a `disc://` device, an `iso://`/plain ISO path, etc.
    /// (Resolved through libfreemkv's URL layer.)
    pub source: String,
    /// Destination: a directory, a file, or a `scheme://` sink (`null://`,
    /// `m2ts://`, …).
    pub dest: String,
    /// Which titles to include.
    pub selection: Selection,
    /// Recovery strategy.
    pub mode: RipMode,
    /// Skip decryption and write ciphertext through (forensic / raw backup).
    pub raw: bool,
    /// Which audio + subtitle streams to keep in each ripped title (video is
    /// always kept). One bundle so the two travel together. Default keeps
    /// everything (archival).
    pub streams: StreamChoice,
}

/// The audio + subtitle stream choice for a rip — the two selections that
/// travel together (from the CLI's `-a`/`-s`, autorip's config, the desktop
/// UI's checkboxes). [`resolve`](StreamChoice::resolve) turns it into the
/// library's PID primitive for one scanned title.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamChoice {
    /// Audio streams to keep. Default [`StreamFilter::All`].
    pub audio: StreamFilter,
    /// Subtitle streams to keep, as two independent sides: full subtitles and
    /// forced ones. Default keeps everything on both sides.
    ///
    /// This is a [`SubtitleFilter`] rather than a plain [`StreamFilter`] because
    /// "German subtitles, forced only if English" is one coherent request that a
    /// single language list cannot express — the forced side is a different
    /// editorial object. A caller that only has one list assigns it directly
    /// (`subtitles: my_filter.into()`); the [`From`] impl applies it to BOTH
    /// sides, which is exactly the pre-forced meaning. It is one field, not two,
    /// so the two sides cannot drift out of sync or be partially updated.
    pub subtitles: SubtitleFilter,
}

impl StreamChoice {
    /// True when every class keeps everything — the apply/resolve is a no-op and
    /// callers can skip it entirely (byte-identical to no selection).
    ///
    /// BOTH subtitle sides count. A choice that keeps all full subtitles but no
    /// forced ones is a real filter: reporting it as "all" would make callers
    /// take the skip path and ship the forced subtitles the user excluded.
    pub fn is_all(&self) -> bool {
        matches!(self.audio, StreamFilter::All)
            && matches!(self.subtitles.normal, StreamFilter::All)
            && matches!(self.subtitles.forced, StreamFilter::All)
    }
}

/// Which streams of one class (audio or subtitle) to keep in a ripped title.
/// The engine translates this to a `libfreemkv::StreamSelection` (PIDs) per
/// scanned title via [`crate::resolve_stream_selection`]. A [`Job`] stays pure
/// data — language tags are raw here and normalized at resolve time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StreamFilter {
    /// Keep every stream of the class (today's behavior; the archival default).
    #[default]
    All,
    /// Keep no streams of the class (video-only when both classes are `None`).
    None,
    /// Keep streams whose language matches any listed tag. Tags are raw user
    /// input — a name (`"English"`), 639-1 (`"en"`), or 639-2/3 (`"eng"`) —
    /// normalized by language identity at resolve time (case-insensitive).
    Langs(Vec<String>),
}

impl Job {
    /// A minimal single-pass job: main movie, decrypt on, no loss tolerance
    /// knob (single-pass ignores it).
    pub fn new(source: impl Into<String>, dest: impl Into<String>) -> Self {
        Job {
            source: source.into(),
            dest: dest.into(),
            selection: Selection::default(),
            mode: RipMode::default(),
            raw: false,
            streams: StreamChoice::default(),
        }
    }

    /// Builder: set the recovery mode.
    pub fn with_mode(mut self, mode: RipMode) -> Self {
        self.mode = mode;
        self
    }

    /// Builder: set the title selection.
    pub fn with_selection(mut self, sel: Selection) -> Self {
        self.selection = sel;
        self
    }

    /// Builder: set the audio stream selection.
    pub fn with_audio(mut self, audio: StreamFilter) -> Self {
        self.streams.audio = audio;
        self
    }

    /// Builder: set the subtitle stream selection.
    ///
    /// Accepts either a plain [`StreamFilter`] — applied to full AND forced
    /// subtitles alike, i.e. forcedness ignored, the pre-forced meaning — or a
    /// [`SubtitleFilter`] with the two sides set independently.
    ///
    /// This REPLACES both sides, so call it before [`Job::with_forced_subtitles`]
    /// if you use both.
    pub fn with_subtitles(mut self, subtitles: impl Into<SubtitleFilter>) -> Self {
        self.streams.subtitles = subtitles.into();
        self
    }

    /// Builder: set only the FORCED-subtitle side, leaving full subtitles alone.
    ///
    /// "Keep every subtitle, but forced ones only in English" is
    /// `with_forced_subtitles(StreamFilter::Langs(vec!["en".into()]))`; "never
    /// give me forced subtitles" is `with_forced_subtitles(StreamFilter::None)`.
    pub fn with_forced_subtitles(mut self, forced: StreamFilter) -> Self {
        self.streams.subtitles.forced = forced;
        self
    }

    /// Builder: set the whole audio+subtitle stream choice at once.
    pub fn with_streams(mut self, streams: StreamChoice) -> Self {
        self.streams = streams;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_common_case() {
        let j = Job::new("disc:///dev/sr0", "/out");
        assert_eq!(j.mode, RipMode::Single);
        assert_eq!(j.selection, Selection::MainMovie);
        assert!(!j.raw);
    }

    #[test]
    fn builders_compose() {
        let j = Job::new("iso://x.iso", "null://")
            .with_mode(RipMode::Multi)
            .with_selection(Selection::All);
        assert_eq!(j.mode, RipMode::Multi);
        assert_eq!(j.selection, Selection::All);
        assert_eq!(j.source, "iso://x.iso");
        assert_eq!(j.dest, "null://");
    }

    #[test]
    fn explicit_title_selection_round_trips() {
        let j = Job::new("d", "o").with_selection(Selection::Titles(vec![0, 2, 5]));
        assert_eq!(j.selection, Selection::Titles(vec![0, 2, 5]));
    }

    /// `is_all` is the "skip the stream filter entirely" shortcut, so it has
    /// to mean BOTH classes keep everything. A version that answers true for a
    /// half-filtered choice makes callers skip a filter the user asked for and
    /// ship every track; one that answers false costs only a redundant
    /// resolve. Untested until now, in either direction.
    #[test]
    fn is_all_requires_both_classes_to_keep_everything() {
        let all = StreamChoice {
            audio: StreamFilter::All,
            subtitles: StreamFilter::All.into(),
        };
        assert!(all.is_all());
        assert!(
            StreamChoice::default().is_all(),
            "the default choice keeps everything"
        );
        assert!(
            !StreamChoice {
                audio: StreamFilter::None,
                subtitles: StreamFilter::All.into(),
            }
            .is_all(),
            "audio is filtered — this is not a no-op"
        );
        assert!(
            !StreamChoice {
                audio: StreamFilter::All,
                subtitles: StreamFilter::None.into(),
            }
            .is_all(),
            "subtitles are filtered — this is not a no-op"
        );
        assert!(
            !StreamChoice {
                audio: StreamFilter::Langs(vec!["eng".into()]),
                subtitles: StreamFilter::Langs(vec!["eng".into()]).into(),
            }
            .is_all()
        );
    }

    /// The default has to keep EVERYTHING on every side, spelled out per side
    /// rather than only through `is_all()` — `is_all` and the default are the
    /// two halves of "an unconfigured job is byte-identical to no selection at
    /// all", and checking one through the other lets a default that quietly
    /// dropped forced subtitles agree with an `is_all` that quietly ignored
    /// them.
    #[test]
    fn default_keeps_every_side() {
        let d = StreamChoice::default();
        assert_eq!(d.audio, StreamFilter::All);
        assert_eq!(d.subtitles.normal, StreamFilter::All, "full subtitles");
        assert_eq!(d.subtitles.forced, StreamFilter::All, "forced subtitles");
        assert!(d.is_all());
        assert_eq!(
            Job::new("disc:///dev/sr0", "/out").streams,
            d,
            "a fresh Job is the archival default"
        );
    }

    /// The bug the forced side creates the moment it exists: `is_all()` that
    /// only looks at the normal side answers TRUE for "every full subtitle, no
    /// forced ones". Callers use `is_all()` to SKIP resolving entirely, so that
    /// answer ships exactly the forced subtitles the user excluded — silently,
    /// at exit 0.
    #[test]
    fn is_all_is_false_when_only_the_forced_side_is_narrowed() {
        let no_forced = StreamChoice {
            audio: StreamFilter::All,
            subtitles: SubtitleFilter::split(StreamFilter::All, StreamFilter::None),
        };
        assert!(
            !no_forced.is_all(),
            "forced subtitles are excluded — this is not a no-op"
        );

        let forced_english = StreamChoice {
            audio: StreamFilter::All,
            subtitles: SubtitleFilter::split(
                StreamFilter::All,
                StreamFilter::Langs(vec!["en".into()]),
            ),
        };
        assert!(
            !forced_english.is_all(),
            "the forced side is language-filtered — this is not a no-op"
        );

        // The mirror: narrowing only the NORMAL side must stay false too, so a
        // fix that merely swapped which side is inspected cannot pass.
        let no_full = StreamChoice {
            audio: StreamFilter::All,
            subtitles: SubtitleFilter::split(StreamFilter::None, StreamFilter::All),
        };
        assert!(!no_full.is_all(), "full subtitles are excluded");
    }

    /// A caller that has only ever had ONE subtitle list must keep today's
    /// meaning exactly: the list applies to full and forced subtitles alike.
    /// If `From` ever set only one side, `-s eng` would start dropping the
    /// English forced subtitle it used to keep.
    #[test]
    fn a_plain_subtitle_list_applies_to_both_sides() {
        for f in [
            StreamFilter::All,
            StreamFilter::None,
            StreamFilter::Langs(vec!["deu".into()]),
        ] {
            let via_builder = Job::new("d", "o")
                .with_subtitles(f.clone())
                .streams
                .subtitles;
            assert_eq!(via_builder.normal, f, "normal side");
            assert_eq!(via_builder.forced, f, "forced side");
            assert_eq!(
                via_builder,
                SubtitleFilter::from(f.clone()),
                "the builder must go through the same conversion"
            );
        }
    }

    /// `with_forced_subtitles` narrows ONLY the forced side — the whole point of
    /// the split is that the two are set independently.
    #[test]
    fn forced_builder_leaves_the_normal_side_alone() {
        let j = Job::new("d", "o")
            .with_subtitles(StreamFilter::Langs(vec!["deu".into()]))
            .with_forced_subtitles(StreamFilter::Langs(vec!["eng".into()]));
        assert_eq!(
            j.streams.subtitles.normal,
            StreamFilter::Langs(vec!["deu".into()])
        );
        assert_eq!(
            j.streams.subtitles.forced,
            StreamFilter::Langs(vec!["eng".into()])
        );
        assert!(!j.streams.is_all());
    }

    fn audio_stream(pid: u16, lang: &str) -> libfreemkv::Stream {
        libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid,
            codec: libfreemkv::Codec::TrueHd,
            channels: libfreemkv::AudioChannels::Stereo,
            language: lang.into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })
    }

    fn sub_stream(pid: u16, lang: &str, forced: bool) -> libfreemkv::Stream {
        libfreemkv::Stream::Subtitle(libfreemkv::SubtitleStream {
            pid,
            codec: libfreemkv::Codec::Pgs,
            language: lang.into(),
            forced,
            qualifier: libfreemkv::LabelQualifier::None,
            codec_data: None,
        })
    }

    /// deu and eng each exist on BOTH sides of the forced flag, so a resolve
    /// that ignored the flag would keep two PIDs where one is correct.
    fn split_title() -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.streams = vec![
            audio_stream(0x1100, "deu"),
            sub_stream(0x1200, "deu", false),
            sub_stream(0x1201, "eng", false),
            sub_stream(0x1210, "deu", true),
            sub_stream(0x1211, "eng", true),
        ];
        t
    }

    /// End-to-end through the type a front-end actually holds: a `StreamChoice`
    /// carrying a split must reach the forced-aware resolver, so the two sides
    /// select independently. If `resolve` collapsed the choice back to one list
    /// (either side), this asks for German full subtitles and English forced
    /// ones and would get both German PIDs or both English ones instead.
    #[test]
    fn resolve_routes_the_two_subtitle_sides_independently() {
        let choice = StreamChoice {
            audio: StreamFilter::None,
            subtitles: SubtitleFilter::split(
                StreamFilter::Langs(vec!["de".into()]),
                StreamFilter::Langs(vec!["en".into()]),
            ),
        };
        let sel = choice.resolve(&split_title()).unwrap();
        assert_eq!(
            sel.subtitle,
            libfreemkv::PidFilter::Only(vec![0x1200, 0x1211]),
            "German FULL subtitle union English FORCED subtitle"
        );

        // Swap the sides: the answer must swap too, so neither side can be the
        // one that silently drives both.
        let swapped = StreamChoice {
            audio: StreamFilter::None,
            subtitles: SubtitleFilter::split(
                StreamFilter::Langs(vec!["en".into()]),
                StreamFilter::Langs(vec!["de".into()]),
            ),
        };
        assert_eq!(
            swapped.resolve(&split_title()).unwrap().subtitle,
            libfreemkv::PidFilter::Only(vec![0x1201, 0x1210])
        );

        // And a single list still ignores forcedness: both German PIDs.
        let plain = StreamChoice {
            audio: StreamFilter::None,
            subtitles: StreamFilter::Langs(vec!["de".into()]).into(),
        };
        assert_eq!(
            plain.resolve(&split_title()).unwrap().subtitle,
            libfreemkv::PidFilter::Only(vec![0x1200, 0x1210])
        );
    }
}
