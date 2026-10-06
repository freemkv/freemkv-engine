//! Drive data capture — read hardware information via SCSI.

use anyhow::Result;
use libfreemkv::Drive;

/// Raw data captured from a drive's SCSI responses.
#[derive(Clone)]
pub struct DriveCapture {
    /// Raw INQUIRY response (96 bytes)
    pub inquiry: Vec<u8>,
    /// Raw GET_CONFIG 010C response
    pub gc_010c: Vec<u8>,
    /// GET_CONFIG feature responses: (feature_code, feature_name, data)
    pub features: Vec<CapturedFeature>,
    /// REPORT_KEY RPC state
    pub rpc_state: Option<Vec<u8>>,
    /// MODE SENSE page 2A (capabilities)
    pub mode_2a: Option<Vec<u8>>,
    /// READ_BUFFER 0xF1 (Pioneer vendor data)
    pub rb_f1: Option<Vec<u8>>,
    /// READ_BUFFER mode 6 (MTK vendor data)
    pub rb_mode6: Option<Vec<u8>>,
    /// READ_BUFFER 0xB0 @0x04 — low vendor window, captured BEFORE the knock
    /// (Renesas signature only). GOOD status = already readable.
    pub rb_b0_04: Option<Vec<u8>>,
    /// READ_BUFFER 0xB0 @0x500000 — high vendor "state" window, captured BEFORE
    /// the knock (Renesas signature only).
    pub rb_b0_500000: Option<Vec<u8>>,
    /// WRITE_BUFFER 0x41 @0xA5AAAA — the payload-less vendor "knock". Issued
    /// UNCONDITIONALLY here as the before/after diagnostic — NOT gated on the open
    /// read failing — so its effect on both windows can be observed. `Some(empty)`
    /// on GOOD status.
    pub wb_41: Option<Vec<u8>>,
    /// READ_BUFFER 0xB0 @0x04 re-read AFTER the knock. Diff against `rb_b0_04`.
    pub rb_b0_04_postknock: Option<Vec<u8>>,
    /// READ_BUFFER 0xB0 @0x500000 re-read AFTER the knock. Diff against
    /// `rb_b0_500000`: if this window only turns GOOD (or changes content) after
    /// the knock, it is the access-gated data window the knock unlocks; if it was
    /// already GOOD before, the knock is a status/bank op instead.
    pub rb_b0_500000_postknock: Option<Vec<u8>>,
    /// READ_BUFFER 0xF4 (256 B) — the Pioneer firmware/drive-flags vendor read
    /// (Renesas signature only). Content unvalidated; may be empty or refused
    /// depending on drive state.
    pub rb_f4: Option<Vec<u8>>,
}

/// A single GET CONFIGURATION feature response from the drive.
#[derive(Clone)]
pub struct CapturedFeature {
    /// MMC-6 GET CONFIGURATION feature code (e.g. `0x010D` = AACS).
    pub code: u16,
    /// Static human-readable label from the internal `FEATURES` table —
    /// not a device-reported string.
    pub name: &'static str,
    /// Raw feature-descriptor payload bytes, with the 8-byte GET
    /// CONFIGURATION header stripped (i.e. `buf[8..]`). Unlike
    /// [`DriveCapture::gc_010c`], which retains the full header.
    pub data: Vec<u8>,
}

// Hand-written so `{:?}` can't leak unredacted identifying bytes (e.g. the
// INQUIRY serial, or the Serial Number feature). Every raw field is run through
// `mask_bytes` — the same policy the `--share` report applies.
impl std::fmt::Debug for DriveCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mo = |o: &Option<Vec<u8>>| o.as_deref().map(mask_bytes);
        f.debug_struct("DriveCapture")
            .field("inquiry", &mask_bytes(&self.inquiry))
            .field("gc_010c", &mask_bytes(&self.gc_010c))
            .field("features", &self.features)
            .field("rpc_state", &mo(&self.rpc_state))
            .field("mode_2a", &mo(&self.mode_2a))
            .field("rb_f1", &mo(&self.rb_f1))
            .field("rb_mode6", &mo(&self.rb_mode6))
            .field("rb_b0_04", &mo(&self.rb_b0_04))
            .field("rb_b0_500000", &mo(&self.rb_b0_500000))
            .field("wb_41", &mo(&self.wb_41))
            .field("rb_b0_04_postknock", &mo(&self.rb_b0_04_postknock))
            .field("rb_b0_500000_postknock", &mo(&self.rb_b0_500000_postknock))
            .field("rb_f4", &mo(&self.rb_f4))
            .finish()
    }
}

impl std::fmt::Debug for CapturedFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedFeature")
            .field("code", &self.code)
            .field("name", &self.name)
            .field("data", &mask_bytes(&self.data))
            .finish()
    }
}

/// Feature codes to capture.
const FEATURES: &[(u16, &str)] = &[
    (0x0000, "Profile List"),
    (0x0001, "Core"),
    (0x0003, "Removable Medium"),
    (0x0010, "Random Readable"),
    (0x001D, "Multi-Read"),
    (0x001E, "CD Read"),
    (0x001F, "DVD Read"),
    (0x0040, "BD Read"),
    (0x0041, "BD Write"),
    (0x0100, "Power Management"),
    (0x0102, "Embedded Changer"),
    (0x0107, "Real Time Streaming"),
    (0x0108, "Serial Number"),
    (0x010C, "Firmware Information"),
    (0x010D, "AACS"),
];

/// Capture all available drive data via SCSI commands.
/// Returns raw responses — no formatting, no zipping, no presentation.
pub fn capture_drive_data(session: &mut Drive) -> Result<DriveCapture> {
    let id = &session.drive_id;

    // Already have INQUIRY from drive open
    let inquiry = id.raw_inquiry.clone();
    let gc_010c = id.raw_gc_010c.clone();

    // Capture GET_CONFIG features using Drive's query methods
    let mut features = Vec::new();
    for &(code, name) in FEATURES {
        if let Some(data) = session.get_config_feature(code) {
            features.push(CapturedFeature { code, name, data });
        }
    }

    // Vendor-specific READ_BUFFER queries
    let rb_f1 = session.read_buffer(0x02, 0xF1, 48); // Pioneer
    let rb_mode6 = session.read_buffer(0x06, 0x00, 32); // MTK

    // Renesas "SAT" signature (RB 0xF1 [16..19]). --share reads both vendor
    // windows + the 0xF4 window before and after a payload-less knock (WB 0x41
    // @0xA5AAAA); the before/after diff characterizes what the knock unlocks.
    let (mut rb_b0_04, mut rb_b0_500000) = (None, None);
    let (mut wb_41, mut rb_b0_04_postknock, mut rb_b0_500000_postknock) = (None, None, None);
    let mut rb_f4 = None;
    if rb_f1.as_deref().is_some_and(is_renesas_sat) {
        // BEFORE: both windows + the F4 firmware window.
        rb_b0_04 = read_window(session, RB_B0_04);
        rb_b0_500000 = read_window(session, RB_B0_500000);
        rb_f4 = read_window(session, RB_F4);
        // KNOCK: the universal enable (payload-less write). `Some(empty)` = GOOD.
        wb_41 = raw(session, &KNOCK, libfreemkv::scsi::DataDirection::None, 0);
        // AFTER: re-read both windows to observe the knock's effect.
        rb_b0_04_postknock = read_window(session, RB_B0_04);
        rb_b0_500000_postknock = read_window(session, RB_B0_500000);
    }

    // Standard queries
    let rpc_state = session.report_key_rpc_state();
    let mode_2a = session.mode_sense_page(0x2A);

    Ok(DriveCapture {
        inquiry,
        gc_010c,
        features,
        rpc_state,
        mode_2a,
        rb_f1,
        rb_mode6,
        rb_b0_04,
        rb_b0_500000,
        wb_41,
        rb_b0_04_postknock,
        rb_b0_500000_postknock,
        rb_f4,
    })
}

/// A Renesas READ_BUFFER (mode 0x02) window: `(buffer id, offset, length)`.
type Window = (u8, u32, u32);
const RB_B0_04: Window = (0xB0, 0x04, 164);
const RB_B0_500000: Window = (0xB0, 0x50_0000, 164);
const RB_F4: Window = (0xF4, 0x00, 256);
/// WRITE BUFFER mode 0x02, id 0x41, offset 0xA5AAAA, no payload.
const KNOCK: [u8; 10] = [0x3B, 0x02, 0x41, 0xA5, 0xAA, 0xAA, 0, 0, 0, 0];

/// Read one Renesas window; the CDB's length and the buffer size come from one value.
fn read_window(session: &mut Drive, (id, offset, len): Window) -> Option<Vec<u8>> {
    let cdb = libfreemkv::scsi::build_read_buffer(0x02, id, offset, len);
    raw(
        session,
        &cdb,
        libfreemkv::scsi::DataDirection::FromDevice,
        len as usize,
    )
}

/// Renesas signature: `SAT` at bytes 16..19 of the READ_BUFFER 0xF1 response.
fn is_renesas_sat(rb_f1: &[u8]) -> bool {
    rb_f1.get(16..19) == Some(b"SAT".as_slice())
}

/// Run a raw CDB; `Some(data)` on GOOD status (empty for a write), else `None`.
fn raw(
    session: &mut Drive,
    cdb: &[u8],
    dir: libfreemkv::scsi::DataDirection,
    len: usize,
) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let r = match session.scsi_execute(cdb, dir, &mut buf, 5_000) {
        Ok(r) => r,
        Err(e) => {
            // A transport fault, not a refusal: say so, or the capture reads as complete.
            tracing::warn!(target: "freemkv::engine", "drive capture: CDB {:02x} failed: {e}", cdb[0]);
            return None;
        }
    };
    (r.status == 0).then(|| buf[..r.bytes_transferred.min(buf.len())].to_vec())
}

/// The one masking policy both `mask_string` and `mask_bytes` apply, so a
/// future change (e.g. also masking punctuation) can't drift between them.
/// ASCII letter -> 'A', ASCII digit -> '0', everything else unchanged.
fn mask_char(c: char) -> char {
    if c.is_ascii_alphabetic() {
        'A'
    } else if c.is_ascii_digit() {
        '0'
    } else {
        c
    }
}

/// Mask a string for privacy (letters->A, digits->0).
pub fn mask_string(s: &str) -> String {
    s.chars().map(mask_char).collect()
}

/// Mask bytes for privacy. A byte round-trips through `char` (Latin-1), so the
/// ASCII classification — the only thing `mask_char` acts on — is identical.
pub fn mask_bytes(data: &[u8]) -> Vec<u8> {
    data.iter().map(|&b| mask_char(b as char) as u8).collect()
}

#[cfg(test)]
#[path = "drive_info_tests.rs"]
mod tests;
