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
/// - AACS with a resolved state → resolved, origin carried, summary keyed to
///   the origin (`"resolved-keydb"`, `"resolved-online"`, …).
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
        let summary = match aacs.key_source {
            libfreemkv::KeyOrigin::KeyDb
            | libfreemkv::KeyOrigin::KeyDbDerived
            | libfreemkv::KeyOrigin::KeyDbUnitKeys => "resolved-keydb",
            libfreemkv::KeyOrigin::ExternalUk => "resolved-online",
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
    fn aacs_external_uk_is_online() {
        let mut d = disc(true);
        d.aacs = Some(aacs(libfreemkv::KeyOrigin::ExternalUk));
        assert_eq!(resolve_keys(&d).summary, "resolved-online");
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
