//! What to rip, and how — the front-end's request to the engine.
//!
//! A [`Job`] is pure data: a front-end builds one from CLI args, a web POST, or
//! GUI selections, hands it to [`crate::preflight()`] to check it, then to an
//! entry point ([`crate::multipass_rip`], [`crate::recover_to_iso`],
//! [`crate::mux_title`]) to execute it. It carries no I/O handles and no
//! callbacks — those arrive separately as the [`crate::Sink`].

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
    /// A TV disc's episodes: the similar-length cluster, without the "play all"
    /// title, extras, or duplicate angles.
    Episodes,
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
    /// The rip's up-front key set ([`crate::keys::resolve_for_rip`], KU §2.1), in memory
    /// only. `None` holds no key: a decrypting rip of an AACS disc refuses (E7022) whatever
    /// keys the disc banked (KU-X1).
    pub keys: Option<libfreemkv::keys::KeyRing>,
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
    /// A minimal single-pass job: main movie, decrypt on. The loss-tolerance
    /// knob lives on `MultipassOpts` (passed to `multipass_rip`), not on
    /// `Job` — a single-pass job has none to set.
    pub fn new(source: impl Into<String>, dest: impl Into<String>) -> Self {
        Job {
            source: source.into(),
            dest: dest.into(),
            selection: Selection::default(),
            mode: RipMode::default(),
            raw: false,
            streams: StreamChoice::default(),
            keys: None,
        }
    }

    /// Builder: the rip's up-front key set, read by every decrypting pass and gate.
    pub fn with_keys(mut self, keys: libfreemkv::keys::KeyRing) -> Self {
        self.keys = Some(keys);
        self
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
#[path = "job_tests.rs"]
mod tests;
