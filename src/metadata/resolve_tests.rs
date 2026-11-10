use super::layout::{relative_path, title_with_year};
use super::*;
use std::path::{Component, Path};

fn meta(title: &str, year: u16, kind: Option<MediaKind>) -> MediaMetadata {
    MediaMetadata {
        title: title.into(),
        year,
        kind,
        ..MediaMetadata::default()
    }
}

#[test]
fn resolve_takes_each_field_from_operator_then_tmdb() {
    let mut operator = meta("Typed", 0, None);
    operator.season = Some(3);
    let mut tmdb = meta("Matched", 2004, Some(MediaKind::Tv));
    tmdb.tmdb_id = 42;
    tmdb.season = Some(2);
    tmdb.disc = Some(2);
    let got = MediaMetadata::resolve(&operator, Some(&tmdb), &["SHOW_S5_D4"]);
    assert_eq!(
        got,
        MediaMetadata {
            title: "Typed".into(),
            year: 2004,
            tmdb_id: 42,
            season: Some(3),
            disc: Some(2),
            episode_start: None,
            kind: Some(MediaKind::Tv),
        }
    );
    assert_eq!(
        MediaMetadata::resolve(&MediaMetadata::default(), None, &[]),
        MediaMetadata::default()
    );
}

#[test]
fn a_season_label_makes_tv_and_supplies_season_and_disc() {
    let operator = meta("Samurai Champloo", 0, None);
    let got = MediaMetadata::resolve(&operator, None, &["", "SAMURAI_S1_D2"]);
    assert_eq!(got.kind, Some(MediaKind::Tv));
    assert_eq!((got.season, got.disc), (Some(1), Some(2)));
    // The first label naming a number wins, per number.
    let got = MediaMetadata::resolve(&operator, None, &["Show Disc 3", "Show Season 2 Disc 5"]);
    assert_eq!((got.season, got.disc), (Some(2), Some(3)));
    // An operator TV choice reads label numbers it left open.
    let mut tv = meta("Show", 0, Some(MediaKind::Tv));
    tv.season = Some(4);
    let got = MediaMetadata::resolve(&tv, None, &["SHOW_D2"]);
    assert_eq!((got.season, got.disc), (Some(4), Some(2)));
    // A label without a season is no evidence of TV, and its disc number is not read.
    let got = MediaMetadata::resolve(&operator, None, &["MOVIE_DISC_2"]);
    assert_eq!((got.kind, got.disc), (None, None));
}

// "Correct it to a movie" must keep working on a season-marked label.
#[test]
fn an_operator_movie_overrides_a_season_label() {
    let mut operator = meta("Not A Show", 0, Some(MediaKind::Movie));
    operator.episode_start = Some(5);
    let tmdb = meta("Show", 2001, Some(MediaKind::Tv));
    for tmdb in [None, Some(&tmdb)] {
        let got = MediaMetadata::resolve(&operator, tmdb, &["SHOW_SEASON_2_DISC_1"]);
        assert_eq!(got.kind, Some(MediaKind::Movie));
        assert_eq!(
            (got.season, got.disc, got.episode_start),
            (None, None, None)
        );
    }
    // A TMDB Movie alone stays a movie; pass `None` when nothing matched.
    let movie = meta("Film", 2001, Some(MediaKind::Movie));
    let got = MediaMetadata::resolve(&MediaMetadata::default(), Some(&movie), &["FILM"]);
    assert_eq!((got.kind, got.season), (Some(MediaKind::Movie), None));
}

// A TV disc TMDB mismatched to a film stays TV: the season label outranks a TMDB Movie.
#[test]
fn a_season_label_outranks_a_tmdb_movie() {
    let movie = meta("Film", 2001, Some(MediaKind::Movie));
    let got = MediaMetadata::resolve(&MediaMetadata::default(), Some(&movie), &["X_S2_D3"]);
    assert_eq!(got.kind, Some(MediaKind::Tv));
    assert_eq!((got.season, got.disc), (Some(2), Some(3)));
    assert_eq!((got.title.as_str(), got.year), ("Film", 2001));
    // TMDB TV with no season label is TV; an operator Movie still beats both.
    let show = meta("Show", 2001, Some(MediaKind::Tv));
    let got = MediaMetadata::resolve(&MediaMetadata::default(), Some(&show), &["SHOW"]);
    assert_eq!((got.kind, got.season), (Some(MediaKind::Tv), None));
    let operator = meta("", 0, Some(MediaKind::Movie));
    let got = MediaMetadata::resolve(&operator, Some(&movie), &["X_S2_D3"]);
    assert_eq!(
        (got.kind, got.season, got.disc),
        (Some(MediaKind::Movie), None, None)
    );
}

#[test]
fn tmdb_results_convert_with_their_kind() {
    let result = tmdb::TmdbResult {
        title: "Endeavour".into(),
        year: 2012,
        poster_url: String::new(),
        overview: String::new(),
        media_type: MediaKind::Tv,
        tmdb_id: 7,
    };
    assert_eq!(
        MediaMetadata::from(&result),
        MediaMetadata {
            tmdb_id: 7,
            ..meta("Endeavour", 2012, Some(MediaKind::Tv))
        }
    );
}

// The server's `episode_start` / `check_episode_range` and the desktop's `tv_first_episode`.
#[test]
fn episode_numbers_follow_disc_order_unless_given() {
    assert_eq!(episode_numbers(4, None, None), Ok(1));
    assert_eq!(episode_numbers(4, Some(1), None), Ok(1));
    assert_eq!(episode_numbers(4, Some(0), None), Ok(1));
    assert_eq!(episode_numbers(4, Some(3), None), Ok(9));
    assert_eq!(episode_numbers(4, Some(3), Some(2)), Ok(2));
    assert_eq!(episode_numbers(1, None, Some(u16::MAX)), Ok(u16::MAX));
    for (count, disc, first) in [
        (4, None, Some(0)),
        (0, None, None),
        (0, None, Some(1)),
        (2, None, Some(u16::MAX)),
        (2, Some(u16::MAX), None),
        (usize::MAX, Some(3), None),
        (70_000, None, None),
    ] {
        assert_eq!(
            episode_numbers(count, disc, first),
            Err(EpisodeRangeError),
            "{count} {disc:?} {first:?}"
        );
    }
}

// The mover's `tv_episode_leaf` names, without the extension.
#[test]
fn episode_leaf_matches_the_delivered_name() {
    assert_eq!(episode_leaf("Endeavour", 5, 1, ""), "Endeavour S05E01");
    assert_eq!(
        episode_leaf("Endeavour", 5, 1, "Muse"),
        "Endeavour S05E01 - Muse"
    );
    assert_eq!(episode_leaf("Rogue/One", 1, 2, ""), "RogueOne S01E02");
    assert_eq!(episode_leaf("Show", 1, 123, ""), "Show S01E123");
    for hostile in [
        "..",
        "../../../etc/passwd",
        "..\\..\\Windows\\System32",
        "Part 1/2",
        "C:\\evil",
        "a\0b",
        "Who? *What* <Where> |When| \"Why\"",
        "   ",
        "日本語",
    ] {
        let leaf = format!("{}.mkv", episode_leaf("Show", 1, 2, hostile));
        assert!(leaf.starts_with("Show S01E02 - "), "{leaf:?}");
        let mut parts = Path::new(&leaf).components();
        assert!(
            matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none(),
            "{leaf:?} from {hostile:?}"
        );
        for bad in ['/', '\\', ':', '\0', '*', '?', '"', '<', '>', '|'] {
            assert!(!leaf.contains(bad), "{leaf:?} keeps {bad:?}");
        }
    }
}

#[test]
fn sanitize_display_matches_the_library_names() {
    assert_eq!(sanitize_display("What's Up, Doc?"), "What's Up Doc");
    assert_eq!(sanitize_display("..hidden"), "hidden");
    assert_eq!(sanitize_display(" .. "), "untitled");
    assert_eq!(sanitize_display(""), "untitled");
    assert_eq!(sanitize_display(&"x".repeat(300)).len(), 200);
}

fn rel(meta: &MediaMetadata, leaf: &str) -> String {
    relative_path(meta, leaf)
        .to_string_lossy()
        .replace('\\', "/")
}

// The mover's `build_destination` layout below the movie/TV roots.
#[test]
fn layout_matches_the_library_mover() {
    let movie = meta("Aurora Drift Two", 2024, Some(MediaKind::Movie));
    assert_eq!(
        rel(&movie, "disc.mkv"),
        "Aurora Drift Two (2024)/Aurora Drift Two (2024).mkv"
    );
    assert_eq!(title_with_year(&movie), "Aurora Drift Two (2024)");
    // Unknown kind files as a movie; no year, no suffix; no extension, mkv.
    let drive = meta("Drive (2011) - 4K Ultra HD", 0, None);
    assert_eq!(
        rel(&drive, "Drive (2011) - 4K Ultra HD.m2ts"),
        "Drive 2011 - 4K Ultra HD/Drive 2011 - 4K Ultra HD.m2ts"
    );
    assert_eq!(
        rel(&drive, "title"),
        "Drive 2011 - 4K Ultra HD/Drive 2011 - 4K Ultra HD.mkv"
    );
    let mut tv = meta("Severance", 2022, Some(MediaKind::Tv));
    assert_eq!(
        rel(&tv, "sev_s01e01.mkv"),
        "Severance (2022)/Season 01/sev_s01e01.mkv"
    );
    tv.season = Some(5);
    assert_eq!(
        rel(&tv, "Severance S05E02 - Name.mkv"),
        "Severance (2022)/Season 05/Severance S05E02 - Name.mkv"
    );
    assert_eq!(
        rel(&tv, "../../escape.mkv"),
        "Severance (2022)/Season 05/escape.mkv"
    );
    assert_eq!(
        rel(&meta(" ", 2020, Some(MediaKind::Tv)), "a/b.mkv"),
        "ab.mkv"
    );
    let hostile = meta("../..", 0, Some(MediaKind::Movie));
    assert!(
        relative_path(&hostile, "x.mkv")
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    );
}
