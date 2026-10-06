//! Privacy-masking, Debug and feature-table tests. `capture_drive_data` needs a
//! real SCSI device; only its pure signature check (`is_renesas_sat`) is tested.
//!
//! `mask_string` / `mask_bytes` redact identifying characters before
//! a drive capture leaves the machine: every ASCII letter → 'A',
//! every ASCII digit → '0', everything else (punctuation, spaces,
//! control bytes, non-ASCII) is preserved verbatim so structural
//! framing (offsets, separators) survives for diffing.
use super::*;

#[test]
fn mask_string_letters_become_a_digits_become_zero() {
    // Mixed case letters all collapse to 'A'; digits to '0'.
    assert_eq!(mask_string("HL-DT-ST"), "AA-AA-AA");
    assert_eq!(mask_string("BU40N"), "AA00A");
}

#[test]
fn mask_string_preserves_non_alnum_punctuation_and_space() {
    // Separators and spaces must be preserved so the masked output
    // keeps the same shape as the original (the whole point of a
    // structure-preserving redaction).
    assert_eq!(mask_string("1.04"), "0.00");
    assert_eq!(mask_string("a b-c.d_e"), "A A-A.A_A");
}

#[test]
fn mask_string_preserves_non_ascii_chars() {
    // is_ascii_alphabetic/is_ascii_digit are false for non-ASCII, so multibyte
    // chars pass through unchanged (no mojibake, no panic): ASCII letters → 'A',
    // digits → '0', 'é' preserved.
    assert_eq!(mask_string("café9"), "AAAé0");
}

#[test]
fn mask_bytes_matches_string_masking_for_ascii() {
    // mask_bytes is the byte-wise analogue: letters→b'A', digits→b'0'.
    assert_eq!(mask_bytes(b"HL-DT-ST"), b"AA-AA-AA".to_vec());
    assert_eq!(mask_bytes(b"1.04"), b"0.00".to_vec());
}

#[test]
fn mask_bytes_preserves_non_alnum_and_high_bytes() {
    // Control bytes (0x00), high bytes (0xFF), and punctuation are
    // not ASCII alnum and must survive verbatim — INQUIRY payloads
    // are space-padded binary and the framing must be diffable.
    let input = [0x00u8, b'A', 0x20, b'7', 0xFF, b'-'];
    assert_eq!(mask_bytes(&input), vec![0x00, b'A', 0x20, b'0', 0xFF, b'-']);
}

/// Every byte field populated with distinct, identifying bytes (letters + digits).
fn full_capture() -> DriveCapture {
    let id = |tag: &str| Some(format!("{tag}-SN42X").into_bytes());
    DriveCapture {
        inquiry: b"HL-DT-ST BD1".to_vec(),
        gc_010c: b"FW1.04".to_vec(),
        features: vec![CapturedFeature {
            code: 0x0108,
            name: "Serial Number",
            data: b"KX7L2201".to_vec(),
        }],
        rpc_state: id("rpc"),
        mode_2a: id("m2a"),
        rb_f1: id("f1"),
        rb_mode6: id("mode6"),
        rb_b0_04: id("b004"),
        rb_b0_500000: id("b0500k"),
        wb_41: id("wb41"),
        rb_b0_04_postknock: id("b004post"),
        rb_b0_500000_postknock: id("b0500kpost"),
        rb_f4: id("f4"),
    }
}

/// The hand-written `Debug` exists ONLY to redact. Every raw byte field must render
/// under its own name in masked form and never raw; `{:?}` reaches bug reports.
#[test]
fn drive_capture_debug_renders_and_masks_every_raw_field() {
    let c = full_capture();
    let s = format!("{c:?}");
    assert!(s.starts_with("DriveCapture"), "no usable Debug body: {s:?}");

    let mut fields: Vec<(&str, Vec<u8>, bool)> = vec![
        ("inquiry", c.inquiry.clone(), false),
        ("gc_010c", c.gc_010c.clone(), false),
        ("data", c.features[0].data.clone(), false),
    ];
    for (name, v) in [
        ("rpc_state", &c.rpc_state),
        ("mode_2a", &c.mode_2a),
        ("rb_f1", &c.rb_f1),
        ("rb_mode6", &c.rb_mode6),
        ("rb_b0_04", &c.rb_b0_04),
        ("rb_b0_500000", &c.rb_b0_500000),
        ("wb_41", &c.wb_41),
        ("rb_b0_04_postknock", &c.rb_b0_04_postknock),
        ("rb_b0_500000_postknock", &c.rb_b0_500000_postknock),
        ("rb_f4", &c.rb_f4),
    ] {
        fields.push((name, v.clone().unwrap_or_default(), true));
    }
    for (name, raw, optional) in fields {
        assert!(!raw.is_empty(), "{name} must be populated for this test");
        let wrap = |b: &[u8]| {
            if optional {
                format!("{name}: Some({b:?})")
            } else {
                format!("{name}: {b:?}")
            }
        };
        assert!(
            s.contains(&wrap(&mask_bytes(&raw))),
            "{name} is not rendered masked: {s}"
        );
        assert!(
            !s.contains(&format!("{raw:?}")),
            "{name} leaked through DriveCapture's Debug unmasked: {s}"
        );
    }
}

#[test]
fn renesas_signature_needs_sat_at_bytes_16_to_19() {
    let mut f = vec![0u8; 19];
    f[16..19].copy_from_slice(b"SAT");
    assert!(is_renesas_sat(&f), "exactly 19 bytes ending in SAT matches");
    assert!(!is_renesas_sat(&f[..18]), "18 bytes is too short");
    f.push(0);
    assert!(is_renesas_sat(&f), "trailing bytes after SAT still match");
    f[16..19].copy_from_slice(b"SAX");
    assert!(!is_renesas_sat(&f), "wrong signature must not match");
    f[15..18].copy_from_slice(b"SAT");
    assert!(
        !is_renesas_sat(&f),
        "SAT at the wrong offset must not match"
    );
    assert!(!is_renesas_sat(&[]), "empty response must not match");
}

#[test]
fn captured_feature_debug_renders_and_masks_its_payload() {
    let raw = b"KX7L2201".to_vec();
    let f = CapturedFeature {
        code: 0x0108,
        name: "Serial Number",
        data: raw.clone(),
    };
    let s = format!("{f:?}");
    assert!(
        s.contains("CapturedFeature") && s.contains("Serial Number"),
        "the Debug body produced nothing usable: {s:?}"
    );
    assert!(
        s.contains(&format!("{:?}", mask_bytes(&raw))),
        "the payload is not rendered masked: {s}"
    );
    assert!(
        !s.contains(&format!("{raw:?}")),
        "the raw Serial Number feature payload leaked: {s}"
    );
}

#[test]
fn feature_table_has_no_duplicate_codes() {
    // capture_drive_data iterates FEATURES once per code; a duplicate
    // code would silently capture the same feature twice (and bloat
    // the report). Each MMC-6 feature code must be unique.
    let mut seen = std::collections::HashSet::new();
    for &(code, _name) in FEATURES {
        assert!(seen.insert(code), "duplicate feature code {code:#06x}");
    }
}

#[test]
fn feature_table_includes_aacs_010d() {
    // AACS (0x010D) is the feature that gates UHD decryption capture;
    // it must be in the table or AACS drives capture incompletely.
    assert!(
        FEATURES.iter().any(|&(c, _)| c == 0x010D),
        "AACS feature 0x010D must be captured"
    );
}
