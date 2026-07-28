//! What to rip, and how — the front-end's request to the engine.
//!
//! A [`Job`] is pure data: a front-end builds one from CLI args, a web POST, or
//! GUI selections, hands it to [`crate::preflight`] to check it, then to
//! [`crate::run`] to execute it. It carries no I/O handles and no callbacks —
//! those arrive separately as the [`crate::Sink`].

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
    /// In `Multi` mode, how many seconds of main-movie loss to tolerate before
    /// aborting after retries are exhausted. `0` = require a perfect rip (abort
    /// on ANY loss). Ignored in `Single` mode. (See autorip `abort_on_lost_secs`.)
    pub abort_on_lost_secs: u32,
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
    /// Subtitle streams to keep. Default [`StreamFilter::All`].
    pub subtitles: StreamFilter,
}

impl StreamChoice {
    /// True when both classes keep everything — the apply/resolve is a no-op and
    /// callers can skip it entirely (byte-identical to no selection).
    pub fn is_all(&self) -> bool {
        matches!(self.audio, StreamFilter::All) && matches!(self.subtitles, StreamFilter::All)
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
            abort_on_lost_secs: 0,
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
    pub fn with_subtitles(mut self, subtitles: StreamFilter) -> Self {
        self.streams.subtitles = subtitles;
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
        assert_eq!(j.abort_on_lost_secs, 0);
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
}
