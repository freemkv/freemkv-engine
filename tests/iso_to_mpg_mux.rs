//! Engine leg of GUI = CLI = engine parity for `mpg://` output: an `iso://`
//! DVD source muxes to `mpg://` through [`freemkv_engine::mux_title`], the
//! same entry point the CLI and GUI both call.
//!
//! Builds a minimal clear DVD-Video ISO (one MPEG-2 video + one MPEG-1 Layer
//! II audio stream) with real `VIDEO_TS.IFO`/`VTS_01_0.IFO`/`VTS_01_1.VOB`
//! content over a real directory, synthesized to a UDF image via
//! `libfreemkv::DirImage` — the same technique
//! `decrypting_iso_clear_sectors.rs`'s `tree()` helper uses for a BD image.
//! `DirImage`'s DVD-aware placement (`dirimage::layout::place_video_ts`)
//! reads each IFO's own `VTSTT_VOBS`/menu-VOBS offset and places the
//! matching VOB there, so choosing that offset up front fully determines the
//! layout — no second pass to learn real sector numbers is needed.

use libfreemkv::{DirImage, InputOptions, SectorSource, read_filesystem};

const SECTOR: usize = 2048;

// ── IFO byte builders ────────────────────────────────────────────────────
// Offsets are cited from `libfreemkv::ifo`'s parser (`src/ifo.rs`), which
// documents each against the DVD-Video IFO spec; this is the inverse (write).

/// `VIDEO_TS.IFO` (VMG): magic, one TT_SRPT entry naming VTS 1 / title 1.
fn vmg_bytes() -> Vec<u8> {
    let mut d = vec![0u8; 2 * SECTOR];
    d[0..12].copy_from_slice(b"DVDVIDEO-VMG");
    let tt_srpt_sector = 1u32;
    d[0xC4..0xC8].copy_from_slice(&tt_srpt_sector.to_be_bytes());
    let base = tt_srpt_sector as usize * SECTOR;
    d[base..base + 2].copy_from_slice(&1u16.to_be_bytes()); // num_titles = 1
    let e = base + 8;
    d[e + 2..e + 4].copy_from_slice(&1u16.to_be_bytes()); // chapters
    d[e + 6] = 1; // vts_number
    d[e + 7] = 1; // vts_title_num
    d
}

/// `VTS_01_0.IFO`: magic, `vtstt_vobs` = `ifo_sectors` (the VOB starts
/// immediately after this file — `place_video_ts` honours that offset
/// exactly), one NTSC/4:3 video stream, one MPEG-1 Layer II audio stream
/// (coding mode 2, English), one PGC with one cell spanning the whole VOB.
fn vts_bytes(ifo_sectors: u32, vob_sectors: u32) -> Vec<u8> {
    let mut d = vec![0u8; ifo_sectors as usize * SECTOR];
    d[0..12].copy_from_slice(b"DVDVIDEO-VTS");
    d[0xC4..0xC8].copy_from_slice(&ifo_sectors.to_be_bytes()); // vtstt_vobs
    let pgcit_sector = 2u32;
    d[0xCC..0xD0].copy_from_slice(&pgcit_sector.to_be_bytes());

    // VTS_V_ATR @ 0x200: video_format bits5-4 = 0 (NTSC), aspect bits3-2 = 0 (4:3).
    d[0x200] = 0x00;
    d[0x202..0x204].copy_from_slice(&1u16.to_be_bytes()); // num_audio = 1
    // VTS_A_ATR[0] @ 0x204 (8 bytes): coding_mode bits7-5 = 2 (MPEG-1/2 audio,
    // no extension), sample_rate bits5-4 of byte1 = 0 (48 kHz), channels low3
    // bits of byte1 = 1 (2 channels), language "en".
    let a = 0x204;
    d[a] = 2 << 5;
    d[a + 1] = 1;
    d[a + 2..a + 4].copy_from_slice(b"en");
    d[0x254..0x256].copy_from_slice(&0u16.to_be_bytes()); // num_subs = 0

    let pg = pgcit_sector as usize * SECTOR;
    d[pg..pg + 2].copy_from_slice(&1u16.to_be_bytes()); // num_pgcs = 1
    let pgc_rel: u32 = 0x100;
    d[pg + 8 + 4..pg + 8 + 8].copy_from_slice(&pgc_rel.to_be_bytes());
    let pgc = pg + pgc_rel as usize;
    d[pgc + 0x02] = 1; // nr_of_programs
    d[pgc + 0x03] = 1; // nr_of_cells
    // PGC playback_time: BCD 00:00:05, rate flag bits7-6 = 01 (25 fps).
    d[pgc + 0x04..pgc + 0x08].copy_from_slice(&[0x00, 0x00, 0x05, 0x40]);
    // PGC_AST_CTL[0] @ 0x0C: presence bit15 set, physical stream number 0.
    d[pgc + 0x0C..pgc + 0x0E].copy_from_slice(&0x8000u16.to_be_bytes());
    let pgm_map_rel: u16 = 0xEC;
    let cell_tbl_rel: u16 = 0xF0;
    d[pgc + 0xE6..pgc + 0xE8].copy_from_slice(&pgm_map_rel.to_be_bytes());
    d[pgc + 0xE8..pgc + 0xEA].copy_from_slice(&cell_tbl_rel.to_be_bytes());
    d[pgc + pgm_map_rel as usize] = 1; // program 0 -> first cell 1
    let cell = pgc + cell_tbl_rel as usize;
    d[cell] = 0x00; // category: plain feature cell
    d[cell + 4..cell + 8].copy_from_slice(&[0x00, 0x00, 0x05, 0x40]); // duration
    d[cell + 8..cell + 12].copy_from_slice(&0u32.to_be_bytes()); // first_sector
    d[cell + 20..cell + 24].copy_from_slice(&(vob_sectors - 1).to_be_bytes()); // last_sector
    d
}

// ── VOB (MPEG-2 program stream) byte builders ───────────────────────────────

/// A 14-byte MPEG-2 program stream pack header (ISO/IEC 13818-1 §2.5.3.4):
/// pack_start_code, then SCR/mux_rate whose first SCR byte's top 2 bits are
/// `01` (the MPEG-2, not MPEG-1, pack header marker), zero stuffing.
fn pack_header() -> Vec<u8> {
    vec![
        0x00, 0x00, 0x01, 0xBA, 0x44, 0x00, 0x04, 0x00, 0x04, 0x01, 0x01, 0x89, 0xC3, 0xF8,
    ]
}

/// The 5-byte PTS-only encoding of a PES optional header field (ISO/IEC
/// 13818-1 §2.4.3.7): 4-bit prefix, 3x 15-bit PTS chunks each closed by a
/// marker bit.
fn encode_pts(pts: u64, prefix: u8) -> [u8; 5] {
    [
        (prefix << 4) | (((pts >> 29) & 0x0E) as u8) | 0x01,
        ((pts >> 22) & 0xFF) as u8,
        (((pts >> 14) & 0xFE) as u8) | 0x01,
        ((pts >> 7) & 0xFF) as u8,
        (((pts << 1) & 0xFE) as u8) | 0x01,
    ]
}

/// A PES packet: start code, `stream_id`, length, a PTS-only optional header,
/// then `payload`.
fn pes_packet(stream_id: u8, pts: u64, payload: &[u8]) -> Vec<u8> {
    let pts_bytes = encode_pts(pts, 0x02); // '0010' = PTS only
    let mut p = vec![0x00, 0x00, 0x01, stream_id];
    let len = 3 + pts_bytes.len() + payload.len();
    p.extend_from_slice(&(len as u16).to_be_bytes());
    p.extend_from_slice(&[0x80, 0x80, 0x05]); // marker bits, PTS-only flag, header_data_length=5
    p.extend_from_slice(&pts_bytes);
    p.extend_from_slice(payload);
    p
}

/// A minimal MPEG-2 video elementary stream: sequence header (720x480,
/// 4:3, 29.97 fps) + GOP header + one I-picture, per `mux::codec::mpeg2`'s
/// documented layout (`00 00 01 B3 [h_size:12][v_size:12][aspect:4][rate:4]`,
/// `00 00 01 00 [temporal_ref:10][coding_type:3]`).
fn mpeg2_i_frame_es() -> Vec<u8> {
    let mut es = vec![0x00, 0x00, 0x01, 0xB3]; // sequence header
    let (width, height, aspect, frame_rate) = (720u16, 480u16, 2u8, 4u8); // 4:3, 29.97 fps
    es.push((width >> 4) as u8);
    es.push((((width & 0x0F) as u8) << 4) | ((height >> 8) & 0x0F) as u8);
    es.push((height & 0xFF) as u8);
    es.push((aspect << 4) | (frame_rate & 0x0F));
    es.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
    es.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x00, 0x00, 0x00]); // GOP header
    let coding_type_i = 1u8;
    es.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x00, coding_type_i << 3, 0x00, 0x00]); // I-picture
    es.extend_from_slice(&[0xFF; 16]); // dummy slice payload
    es
}

/// A single MPEG-1 Layer II audio frame: header (sync 0xFFF, MPEG-1, Layer
/// II, no CRC, 128 kbit/s, 48 kHz, stereo) sized to the spec formula
/// `(samples/8) * bitrate / rate` = `144 * 128000 / 48000` = 384 bytes
/// (ISO/IEC 11172-3 §2.4.3.1), zero-filled payload.
fn mp2_frame() -> Vec<u8> {
    let mut f = vec![0xFF, 0xFD, 0x84, 0x00];
    f.resize(384, 0);
    f
}

/// The whole VOB payload: one pack, one video PES, one audio PES, the
/// program end code (ISO/IEC 13818-1 §2.5.3.3, `00 00 01 B9`).
fn vob_bytes() -> Vec<u8> {
    let mut v = pack_header();
    v.extend(pes_packet(0xE0, 90_000, &mpeg2_i_frame_es()));
    v.extend(pes_packet(0xC0, 90_000, &mp2_frame()));
    v.extend_from_slice(&[0x00, 0x00, 0x01, 0xB9]);
    v
}

// ── Directory tree → synthesized UDF ISO ────────────────────────────────────

/// Writes a real `VIDEO_TS` tree to a temp dir, synthesizes it to a UDF image
/// via [`DirImage`] (mirrors `decrypting_iso_clear_sectors.rs`'s `tree()`),
/// and materializes the whole image to `iso_path`.
fn build_dvd_iso(iso_path: &std::path::Path) {
    let vob = vob_bytes();
    let vob_sectors = (vob.len() as u32).div_ceil(SECTOR as u32);
    const IFO_SECTORS: u32 = 4;

    let dir = tempfile::tempdir().unwrap();
    let vts_dir = dir.path().join("VIDEO_TS");
    std::fs::create_dir_all(&vts_dir).unwrap();
    std::fs::write(vts_dir.join("VIDEO_TS.IFO"), vmg_bytes()).unwrap();
    std::fs::write(
        vts_dir.join("VTS_01_0.IFO"),
        vts_bytes(IFO_SECTORS, vob_sectors),
    )
    .unwrap();
    std::fs::write(vts_dir.join("VTS_01_1.VOB"), &vob).unwrap();

    let mut img = DirImage::open(dir.path()).unwrap();
    // Sanity: the VOB really landed where `vtstt_vobs` said it would.
    let fs = read_filesystem(&mut img).unwrap();
    let (vts_lba, _) = fs.file_extents(&mut img, "/VIDEO_TS/VTS_01_0.IFO").unwrap()[0];
    let (vob_lba, _) = fs.file_extents(&mut img, "/VIDEO_TS/VTS_01_1.VOB").unwrap()[0];
    assert_eq!(
        vob_lba,
        vts_lba + IFO_SECTORS,
        "DirImage must place the VOB exactly at the IFO's declared vtstt_vobs offset"
    );

    let cap = SectorSource::capacity_sectors(&img);
    let mut bytes = vec![0u8; cap as usize * SECTOR];
    for lba in 0..cap {
        let at = lba as usize * SECTOR;
        SectorSource::read_sectors(&mut img, lba, 1, &mut bytes[at..at + SECTOR], false).unwrap();
    }
    std::fs::write(iso_path, &bytes).unwrap();
}

// ── The test ─────────────────────────────────────────────────────────────

/// `iso://` DVD source → `mpg://` destination through
/// `freemkv_engine::mux_title` — the engine leg of GUI = CLI = engine parity
/// for the mpg:// output (the CLI and GUI call the same `mux_title`).
#[test]
fn iso_dvd_source_muxes_to_mpg_through_the_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let iso_path = tmp.path().join("source.iso");
    build_dvd_iso(&iso_path);
    let mpg_path = tmp.path().join("out.mpg");

    let source_url = format!("iso://{}", iso_path.display());
    let dest_url = format!("mpg://{}", mpg_path.display());
    let mux_opts = freemkv_engine::mux_options(false);
    let outcome = freemkv_engine::mux_title(
        &source_url,
        &dest_url,
        InputOptions::default(),
        &mux_opts,
        0,
        &freemkv_engine::NoopSink,
    )
    .unwrap_or_else(|e| panic!("iso:// -> mpg:// mux must succeed, got {e}"));

    assert!(outcome.completed, "the mux job must complete");
    assert_eq!(outcome.streams, 2, "one video + one audio stream");
    assert!(outcome.bytes_written > 0);

    let out = std::fs::read(&mpg_path).unwrap();
    assert!(
        out.len() >= 14 && out[0..3] == [0x00, 0x00, 0x01] && out[3] == 0xBA,
        "output must start with an MPEG pack start code"
    );
    assert_eq!(
        out[4] >> 6,
        0b01,
        "byte 4's top 2 bits mark an MPEG-2 (not MPEG-1) pack header"
    );
    assert_eq!(
        out[out.len() - 4..],
        [0x00, 0x00, 0x01, 0xB9],
        "output must end with the MPEG program end code"
    );
    assert!(
        out.windows(4).any(|w| w == [0x00, 0x00, 0x01, 0xE0]),
        "output must contain a PES packet for the video stream id 0xE0"
    );

    // Read the muxed output back and confirm its stream shape matches the
    // source title (one video, one audio, same codecs).
    let mut readback = libfreemkv::input(&dest_url, &InputOptions::default()).unwrap_or_else(|e| {
        panic!("the muxed mpg:// output must itself be a readable source, got {e}")
    });
    let streams = readback.info().streams.clone();
    assert_eq!(streams.len(), 2, "readback must see both streams");
    let codecs: Vec<libfreemkv::Codec> = streams
        .iter()
        .map(|s| match s {
            libfreemkv::Stream::Video(v) => v.codec,
            libfreemkv::Stream::Audio(a) => a.codec,
            libfreemkv::Stream::Subtitle(s) => s.codec,
        })
        .collect();
    assert!(codecs.contains(&libfreemkv::Codec::Mpeg2));
    assert!(codecs.contains(&libfreemkv::Codec::Mp2));

    let mut saw_video_frame = false;
    while let Some(frame) = readback.read().unwrap() {
        if matches!(streams[frame.track], libfreemkv::Stream::Video(_)) {
            saw_video_frame = true;
        }
    }
    assert!(saw_video_frame, "the readback must yield the video frame");
}
