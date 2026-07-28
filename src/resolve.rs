//! Key resolution status as DATA, not log lines.
//!
//! A front-end (the desktop UI's keydb strip, the CLI's status line, autorip's
//! web state) needs to answer "are we decrypting this disc, and from where?"
//! without scraping a log. [`resolve_keys`] reads the scanned [`Disc`]'s
//! already-resolved AACS/CSS state and returns it as a [`KeyStatus`].
//!
//! The actual key *derivation* happens in libfreemkv during `Disc::scan` /
//! `DiscSession::resolve_keys` (that's the mechanics — a lib primitive). This
//! function only reports the resulting state; it performs no I/O and no
//! derivation.

use crate::outcome::KeyStatus;

/// Report the key-resolution state of an already-scanned disc, as data.
///
/// - Unencrypted disc → resolved (nothing to decrypt), summary `"unencrypted"`.
/// - AACS with real key material (unit keys or a VUK) → resolved, origin
///   carried, summary keyed to the origin (`"resolved-keydb"`,
///   `"resolved-external"`, `"resolved-derived"`). AACS state present but with
///   no key material (a VID-only placeholder scan) is treated as unresolved.
/// - CSS with a recovered key → resolved, summary `"resolved-css"`.
/// - Encrypted but no key → unresolved, summary `"no-key"` (or the more
///   specific `"no-keydb"` when the library flagged a missing keydb).
pub fn resolve_keys(disc: &libfreemkv::Disc) -> KeyStatus {
    if !disc.encrypted {
        return KeyStatus {
            resolved: true,
            origin: None,
            keydb_entries: None,
            summary: "unencrypted".to_string(),
        };
    }

    if let Some(aacs) = &disc.aacs {
        // `aacs` being present does NOT mean keys were found. The library
        // synthesizes a PLACEHOLDER AACS state (`KeyOrigin::ExternalUk`, empty
        // `unit_keys`) during a VID-only scan, before any key source is
        // consulted. Decryption consumes `unit_keys` (derivable from a VUK), so
        // the honest "resolved" signal is real key material — non-empty
        // `unit_keys` OR a present VUK — not the origin tag alone. (Reporting
        // `resolved: true` off the origin is the bug the desktop UI had to work
        // around with its own `key_summary`; gating here lets it drop that.)
        let have_key = !aacs.unit_keys.is_empty() || aacs.vuk.is_some();
        if have_key {
            let summary = match aacs.key_source {
                libfreemkv::KeyOrigin::KeyDb
                | libfreemkv::KeyOrigin::KeyDbDerived
                | libfreemkv::KeyOrigin::KeyDbUnitKeys => "resolved-keydb",
                // `ExternalUk` = "unit key supplied by the caller" — it is
                // SOURCE-AGNOSTIC (an online service, a mapfile, a cert file, a
                // built-in). The origin does not imply the network, so we must
                // not label it "online".
                libfreemkv::KeyOrigin::ExternalUk => "resolved-external",
                libfreemkv::KeyOrigin::DeviceKey | libfreemkv::KeyOrigin::ProcessingKey => {
                    "resolved-derived"
                }
            };
            return KeyStatus {
                resolved: true,
                origin: Some(aacs.key_source),
                keydb_entries: None,
                summary: summary.to_string(),
            };
        }
        // AACS state present but no usable key material → fall through to the
        // unresolved/no-key report below (a placeholder VID-only scan lands here).
    }

    if disc.css.is_some() {
        return KeyStatus {
            resolved: true,
            origin: None,
            keydb_entries: None,
            summary: "resolved-css".to_string(),
        };
    }

    // Encrypted, no key. Distinguish "no keydb file present" (the library's
    // most common AACS support case) from a generic no-key so the front-end
    // can point the user at the keydb fix.
    let summary = match &disc.aacs_error {
        Some(libfreemkv::Error::KeydbLoad { .. }) => "no-keydb",
        _ => "no-key",
    };
    KeyStatus::unresolved(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disc(encrypted: bool) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "T".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    fn aacs(origin: libfreemkv::KeyOrigin) -> libfreemkv::AacsState {
        libfreemkv::AacsState {
            version: 1,
            bus_encryption: false,
            mkb_version: None,
            disc_hash: String::new(),
            key_source: origin,
            vuk: None,
            unit_keys: vec![(0, [0u8; 16])],
            read_data_key: None,
            volume_id: [0u8; 16],
            uk_ro: Vec::new(),
            mkb: Vec::new(),
        }
    }

    #[test]
    fn unencrypted_is_resolved() {
        let ks = resolve_keys(&disc(false));
        assert!(ks.resolved);
        assert_eq!(ks.summary, "unencrypted");
    }

    #[test]
    fn aacs_keydb_reports_origin_and_summary() {
        let mut d = disc(true);
        d.aacs = Some(aacs(libfreemkv::KeyOrigin::KeyDb));
        let ks = resolve_keys(&d);
        assert!(ks.resolved);
        assert_eq!(ks.summary, "resolved-keydb");
        assert_eq!(ks.origin, Some(libfreemkv::KeyOrigin::KeyDb));
    }

    #[test]
    fn aacs_external_uk_is_external_not_online() {
        // ExternalUk is source-agnostic — never claim it came from the network.
        let mut d = disc(true);
        d.aacs = Some(aacs(libfreemkv::KeyOrigin::ExternalUk));
        let ks = resolve_keys(&d);
        assert!(ks.resolved);
        assert_eq!(ks.summary, "resolved-external");
    }

    #[test]
    fn placeholder_aacs_without_key_material_is_unresolved() {
        // A VID-only scan stamps KeyOrigin::ExternalUk with EMPTY unit_keys and
        // no VUK before any source is consulted. That is NOT resolved — reporting
        // it as such is the bug the desktop UI had to work around.
        let mut d = disc(true);
        let mut a = aacs(libfreemkv::KeyOrigin::ExternalUk);
        a.unit_keys = Vec::new();
        a.vuk = None;
        d.aacs = Some(a);
        let ks = resolve_keys(&d);
        assert!(
            !ks.resolved,
            "empty unit_keys + no VUK must read as unresolved"
        );
        assert_eq!(ks.summary, "no-key");
    }

    #[test]
    fn aacs_vuk_only_counts_as_resolved() {
        // A VUK with not-yet-expanded unit_keys is still real key material.
        let mut d = disc(true);
        let mut a = aacs(libfreemkv::KeyOrigin::KeyDb);
        a.unit_keys = Vec::new();
        a.vuk = Some([0x11; 16]);
        d.aacs = Some(a);
        assert!(resolve_keys(&d).resolved);
    }

    #[test]
    fn encrypted_no_key_is_unresolved() {
        let ks = resolve_keys(&disc(true));
        assert!(!ks.resolved);
        assert_eq!(ks.summary, "no-key");
    }

    #[test]
    fn missing_keydb_surfaces_no_keydb_summary() {
        let mut d = disc(true);
        d.aacs_error = Some(libfreemkv::Error::KeydbLoad {
            path: "<no keydb in search paths>".to_string(),
        });
        let ks = resolve_keys(&d);
        assert!(!ks.resolved);
        assert_eq!(ks.summary, "no-keydb");
    }
}
