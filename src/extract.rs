//! `extract_tree` — shared orchestration around `Disc::extract_tree`.
//!
//! Every front-end that unpacks a disc's decrypted UDF file tree (the CLI's
//! `dir://` destination, the desktop GUI's "decrypted folder" output) does
//! the exact same two things around the library call: bridge its own
//! cooperative-cancel signal into the `Halt` token `Disc::extract_tree`
//! polls at file/batch boundaries, and hand back the per-file + aggregate
//! result for the shell to render. That bridging (a watcher thread mirroring
//! `Sink::should_cancel()` into a fresh `Halt`, joined before returning even
//! on an unwind) was duplicated between `freemkv/src/pipe.rs` and
//! `freemkv/src/engine.rs`; this is the one copy.
//!
//! Mirrors [`crate::mux::mux_title`]'s should_cancel → Halt bridge.
//!
//! Nothing here prints, formats a locale string, or picks an exit code —
//! those are presentation and stay in the shell. The CLI renders
//! `dir.complete` / `dir.lossy` / `dir.file_lossy` from the returned
//! [`libfreemkv::ExtractResult`] and turns a lossy run into a non-zero exit;
//! the GUI renders through its own `Sink::log`. Both keep their own
//! `--force` / writability gate — `extract_tree` only forwards the `force`
//! flag into `ExtractOptions` exactly as `Disc::extract_tree` expects it.

use crate::sink::Sink;
use std::path::Path;

/// Extract `disc`'s decrypted UDF file tree to `dest`.
///
/// `reader` is consumed for content reads (see [`libfreemkv::Disc::extract_tree`]).
/// `force` mirrors the CLI's `--force`: without it, a non-empty `dest` is
/// refused. `sink`'s [`Sink::should_cancel`] is polled by a scoped watcher
/// thread that cancels a fresh [`libfreemkv::Halt`] the moment it returns
/// true, so a long extraction stops at the next file boundary (the
/// in-flight file is left `.partial`, never a half-written file that looks
/// complete) — the watcher is joined (via `thread::scope`) before this
/// returns, on every path including a panic unwind.
pub fn extract_tree(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    dest: &Path,
    force: bool,
    sink: &dyn Sink,
) -> crate::Result<libfreemkv::ExtractResult> {
    // One should_cancel → halt bridge for the whole engine (see
    // `with_cancel_watcher`), so cancelling even a small extraction is
    // deterministic. No progress channel: both shells poll the final result.
    crate::run::with_cancel_watcher(sink, |halt| {
        let opts = libfreemkv::ExtractOptions {
            force,
            progress: None,
            halt: Some(libfreemkv::Halt::from_arc(halt.clone())),
        };
        disc.extract_tree(reader, dest, &opts)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::NoopSink;
    use std::collections::HashMap;

    // ── Minimal in-memory UDF fixture: one root dir, one file, modeled on
    // libfreemkv's own `disc/extract.rs` fixture (same ECMA-167 byte layout).
    // Duplicated here because those fixture types are private to that crate.

    const PART_START: u32 = 2000;
    const SECTOR: usize = 2048;

    struct MemDisc {
        sectors: HashMap<u32, [u8; SECTOR]>,
    }
    impl MemDisc {
        fn new() -> Self {
            Self {
                sectors: HashMap::new(),
            }
        }
        fn put(&mut self, lba: u32, data: [u8; SECTOR]) {
            self.sectors.insert(lba, data);
        }
        fn put_bytes(&mut self, lba: u32, bytes: &[u8]) {
            for (i, chunk) in bytes.chunks(SECTOR).enumerate() {
                let mut s = [0u8; SECTOR];
                s[..chunk.len()].copy_from_slice(chunk);
                self.put(lba + i as u32, s);
            }
        }
    }
    impl libfreemkv::SectorSource for MemDisc {
        fn read_sectors(
            &mut self,
            lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::Result<usize> {
            let need = count as usize * SECTOR;
            for i in 0..count as u32 {
                let off = i as usize * SECTOR;
                let s = self
                    .sectors
                    .get(&(lba + i))
                    .copied()
                    .unwrap_or([0u8; SECTOR]);
                buf[off..off + SECTOR].copy_from_slice(&s);
            }
            Ok(need)
        }
    }

    fn build_file_icb(size: u32, data_lba: u32) -> [u8; SECTOR] {
        let mut s = [0u8; SECTOR];
        s[0..2].copy_from_slice(&266u16.to_le_bytes()); // Extended File Entry
        s[56..64].copy_from_slice(&(size as u64).to_le_bytes()); // info_length
        s[208..212].copy_from_slice(&0u32.to_le_bytes()); // l_ea
        s[212..216].copy_from_slice(&8u32.to_le_bytes()); // l_ad (one Short AD)
        s[216..220].copy_from_slice(&(size & 0x3FFF_FFFF).to_le_bytes());
        s[220..224].copy_from_slice(&data_lba.to_le_bytes());
        s
    }

    fn build_dir_icb(dir_data_lba: u32, dir_data_len: u32) -> [u8; SECTOR] {
        build_file_icb(dir_data_len, dir_data_lba)
    }

    fn push_fid(buf: &mut Vec<u8>, name: &str, icb_lba: u32, is_dir: bool, is_parent: bool) {
        let start = buf.len();
        let name_field: Vec<u8> = if is_parent {
            Vec::new()
        } else {
            let mut v = vec![0x08u8];
            v.extend_from_slice(name.as_bytes());
            v
        };
        let l_fi = name_field.len();
        let mut fid = vec![0u8; 38];
        fid[0..2].copy_from_slice(&257u16.to_le_bytes()); // Tag ID 257 = FID
        let mut file_chars = 0u8;
        if is_dir {
            file_chars |= 0x02;
        }
        if is_parent {
            file_chars |= 0x08;
        }
        fid[18] = file_chars; // FileCharacteristics
        fid[19] = l_fi as u8; // LengthOfFileIdentifier
        fid[24..28].copy_from_slice(&icb_lba.to_le_bytes()); // ICB LogicalBlockNumber
        fid[36..38].copy_from_slice(&0u16.to_le_bytes()); // l_iu = 0 (no impl-use)
        buf.extend_from_slice(&fid);
        buf.extend_from_slice(&name_field);
        let used = buf.len() - start;
        buf.resize(start + ((used + 3) & !3), 0);
    }

    /// Build a one-file synthetic UDF disc: root dir (icb=10, data=11)
    /// containing a single file `HELLO.TXT` (icb=30, data=31).
    fn synthetic_disc_with_one_file(contents: &[u8]) -> (libfreemkv::Disc, MemDisc) {
        let mut m = MemDisc::new();

        // ECMA-167 skeleton, byte-for-byte what libfreemkv's own extract.rs
        // test fixture lays down.
        let mut avdp = [0u8; SECTOR];
        avdp[0..2].copy_from_slice(&2u16.to_le_bytes());
        m.put(256, avdp);
        let mut pd = [0u8; SECTOR];
        pd[0..2].copy_from_slice(&5u16.to_le_bytes());
        pd[188..192].copy_from_slice(&PART_START.to_le_bytes());
        m.put(32, pd);
        let mut lvd = [0u8; SECTOR];
        lvd[0..2].copy_from_slice(&6u16.to_le_bytes());
        lvd[268..272].copy_from_slice(&1u32.to_le_bytes());
        m.put(33, lvd);
        let mut td = [0u8; SECTOR];
        td[0..2].copy_from_slice(&8u16.to_le_bytes());
        m.put(34, td);
        let mut fsd = [0u8; SECTOR];
        fsd[0..2].copy_from_slice(&256u16.to_le_bytes());
        fsd[404..408].copy_from_slice(&10u32.to_le_bytes()); // root ICB lba
        m.put(PART_START, fsd);

        // Root dir: one FID for itself (parent) + one for the file.
        let mut fids = Vec::new();
        push_fid(&mut fids, "", 10, true, true);
        push_fid(&mut fids, "HELLO.TXT", 30, false, false);
        m.put(PART_START + 10, build_dir_icb(11, fids.len() as u32));
        m.put_bytes(PART_START + 11, &fids);

        // The file itself.
        m.put(PART_START + 30, build_file_icb(contents.len() as u32, 31));
        m.put_bytes(PART_START + 31, contents);

        let disc = libfreemkv::Disc {
            volume_id: "TESTDISC".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 4096,
            capacity_bytes: 4096 * 2048,
            layers: 1,
            titles: Vec::new(),
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: false,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        };
        (disc, m)
    }

    #[test]
    fn clean_synthetic_disc_extracts_complete_with_no_loss() {
        let contents = b"hello from a synthetic disc".to_vec();
        let (disc, mut reader) = synthetic_disc_with_one_file(&contents);
        let out_dir = std::env::temp_dir().join(format!(
            "fmkv-engine-extract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let res = extract_tree(&disc, &mut reader, &out_dir, false, &NoopSink)
            .expect("extraction of a clean synthetic disc should succeed");

        assert!(res.complete, "clean disc extracts complete");
        assert!(!res.halted);
        assert_eq!(res.bytes_unreadable, 0);
        assert_eq!(res.files.len(), 1);
        assert_eq!(res.bytes_good, contents.len() as u64);
        let written = std::fs::read(out_dir.join("HELLO.TXT")).expect("file written");
        assert_eq!(written, contents);

        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn cancel_via_sink_halts_extraction() {
        struct CancelSink;
        impl Sink for CancelSink {
            fn should_cancel(&self) -> bool {
                true
            }
        }
        let contents = vec![0u8; 8192];
        let (disc, mut reader) = synthetic_disc_with_one_file(&contents);
        let out_dir = std::env::temp_dir().join(format!(
            "fmkv-engine-extract-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let res = extract_tree(&disc, &mut reader, &out_dir, false, &CancelSink)
            .expect("a cancelled extraction still returns Ok (halted, not errored)");
        assert!(res.halted, "should_cancel()==true must halt the extraction");

        let _ = std::fs::remove_dir_all(&out_dir);
    }
}
