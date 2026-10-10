//! Opt-in read-only real-disc consumer validation; no mux or live drive access.

use freemkv_engine::{
    Selection, SelectionBasis, SelectionModel, SelectionPreferences, StreamFilter,
};
use std::io::{Read, Seek, SeekFrom};

#[test]
#[ignore = "set FREEMKV_DVD_LAUNCH_ISO to the retained KUNG_FU fixture"]
fn kung_fu_authored_launch_selects_presentation_without_filtering_audio() {
    struct Plain {
        file: std::fs::File,
        bytes: usize,
    }
    impl libfreemkv::SectorSource for Plain {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            b: &mut [u8],
            _: bool,
        ) -> libfreemkv::Result<usize> {
            let size = usize::from(count) * 2048;
            self.bytes += size;
            assert!(self.bytes < 64 * 1024 * 1024);
            self.file
                .seek(SeekFrom::Start(u64::from(lba) * 2048))
                .unwrap();
            self.file.read_exact(&mut b[..size]).unwrap();
            Ok(size)
        }
    }
    let path = std::env::var("FREEMKV_DVD_LAUNCH_ISO").expect("explicit fixture required");
    let file = std::fs::File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    assert_eq!(size, 6_847_942_656);
    let mut source = Plain { file, bytes: 0 };
    let disc = libfreemkv::Disc::scan_image(
        &mut source,
        (size / 2048) as u32,
        &libfreemkv::ScanOptions::default(),
    )
    .unwrap();
    let model = SelectionModel::from_disc(&disc);
    for (language, expected) in [("de", 1), ("eng", 2)] {
        let preferences = SelectionPreferences {
            presentation_language: Some(language.into()),
        };
        let report =
            model.select_with_preferences(&Selection::MainMovie, &StreamFilter::All, &preferences);
        assert_eq!(report.basis, SelectionBasis::AuthoredLaunch);
        assert_eq!(report.indices.len(), 1);
        let title = &disc.titles[report.indices[0]];
        assert_eq!(title.playlist_id, expected);
        assert_eq!(
            model
                .select_with_preferences(
                    &Selection::Titles(vec![0]),
                    &StreamFilter::All,
                    &preferences
                )
                .indices,
            vec![0]
        );
        eprintln!(
            "{language} + Audio All: title {} (index {}), {} extents",
            title.playlist_id,
            report.indices[0],
            title.extents.len()
        );
    }
    assert!(
        model
            .select(&Selection::MainMovie, &StreamFilter::All)
            .requires_review()
    );
    assert!(
        model
            .select(&Selection::Episodes, &StreamFilter::All)
            .requires_review()
    );
    let report = model.select_with_preferences(
        &Selection::MainMovie,
        &StreamFilter::All,
        &SelectionPreferences {
            presentation_language: Some("fr".into()),
        },
    );
    assert!(report.requires_review());
    eprintln!("read-only validation bytes={}", source.bytes);
}
