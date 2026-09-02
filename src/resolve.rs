//! Key resolution status as DATA, not log lines.
//!
//! A front-end (the desktop UI's keydb strip, the CLI's status line, autorip's
//! web state) needs to answer "are we decrypting this disc, and from where?"
//! without scraping a log. [`resolve_keys`] reads the scanned [`libfreemkv::Disc`]'s
//! already-resolved AACS/CSS state and returns it as a [`KeyStatus`].
//!
//! Key *derivation* happens in libfreemkv (`Disc::scan` / `DiscSession::resolve_keys`);
//! this module only reports the resulting state, with no I/O or derivation of its own.

use crate::outcome::KeyStatus;

/// Report the key-resolution state of an already-scanned disc, as data.
///
/// - Unencrypted disc → resolved, summary `"unencrypted"`.
/// - AACS with real key material (unit keys or a VUK) → resolved, origin
///   carried, summary `"resolved-keydb"` / `"resolved-external"` /
///   `"resolved-derived"`. AACS with no key material (VID-only scan) is unresolved.
/// - CSS with a recovered key → resolved, summary `"resolved-css"`.
/// - Encrypted, no key → unresolved, summary `"no-key"` / `"no-keydb"` / a
///   `"key-service-*"` variant (source failed, not proof of "no key").
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
        // `aacs` present does NOT mean keys were found: a PLACEHOLDER AACS state
        // is synthesized during a VID-only scan. "resolved" means real key
        // material — non-empty `unit_keys` OR a present VUK — not the tag alone.
        let have_key = !aacs.unit_keys.is_empty() || aacs.vuk.is_some();
        if have_key {
            let summary = match aacs.key_source {
                libfreemkv::KeyOrigin::KeyDb
                | libfreemkv::KeyOrigin::KeyDbDerived
                | libfreemkv::KeyOrigin::KeyDbUnitKeys => "resolved-keydb",
                // `ExternalUk` = "unit key supplied by the caller" — SOURCE-
                // AGNOSTIC (online service, mapfile, cert file, built-in), so it
                // must not be labeled "online".
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
        // A key SOURCE could not answer. NOT "no-key": nothing was learned about
        // whether this disc has a key, and the operator action is to retry / fix
        // the token / back off, not to go looking for a VUK.
        Some(libfreemkv::Error::KeyServiceUnavailable) => "key-service-unavailable",
        Some(libfreemkv::Error::KeyServiceUnauthorized) => "key-service-unauthorized",
        Some(libfreemkv::Error::KeyServiceRateLimited) => "key-service-rate-limited",
        _ => "no-key",
    };
    KeyStatus::unresolved(summary)
}

// The decrypt gate the executors use, layered on the library gate.
// See docs/resolve.md — closes the `aacs_error`-with-no-`aacs` gap the
// library gate alone lets a disc pass through unresolved.
pub(crate) fn ensure_decryptable_strict(disc: &libfreemkv::Disc, raw: bool) -> crate::Result<()> {
    disc.ensure_decryptable(raw)?;
    if disc.encrypted && !raw && !resolve_keys(disc).resolved {
        // A disc whose key SOURCE failed keeps that verdict here too, covering
        // the `aacs: None, aacs_error: Some(..)` disc the library gate passes,
        // so the two predicates can't disagree about WHY, not just WHETHER.
        match disc.aacs_error {
            Some(libfreemkv::Error::KeyServiceUnavailable) => {
                return Err(libfreemkv::Error::KeyServiceUnavailable);
            }
            Some(libfreemkv::Error::KeyServiceUnauthorized) => {
                return Err(libfreemkv::Error::KeyServiceUnauthorized);
            }
            Some(libfreemkv::Error::KeyServiceRateLimited) => {
                return Err(libfreemkv::Error::KeyServiceRateLimited);
            }
            _ => {}
        }
        return Err(libfreemkv::error::Error::NoDiscKey {
            disc_hash: disc
                .aacs
                .as_ref()
                .map(|a| a.disc_hash.clone())
                .unwrap_or_default(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every `KeyStatus::summary` value this module can emit must be
    // documented where a front-end will look for it, checked from the
    // SOURCE so an undocumented addition fails loudly. See docs/resolve.md.
    #[test]
    fn every_emitted_key_summary_is_documented() {
        let src = include_str!("resolve.rs");
        let guide = include_str!("../USING_THE_ENGINE.md");
        // Scope the scan to `resolve_keys`' own body — anchored structurally,
        // like both sibling tests, so an unrelated string literal elsewhere in
        // the file can neither add a phantom key nor mask a missing one.
        let body = src
            .split_once("pub fn resolve_keys(")
            .expect("the file declares resolve_keys")
            .1
            .split_once("\n}")
            .expect("the function body is closed")
            .0;

        let mut keys: Vec<&str> = Vec::new();
        for line in body.lines() {
            if line.trim_start().starts_with("//") {
                continue; // prose about a key is not an emission of it
            }
            for (i, part) in line.split('"').enumerate() {
                // Odd indices are the insides of string literals.
                if i % 2 == 1
                    && !part.is_empty()
                    && part.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                {
                    keys.push(part);
                }
            }
        }
        keys.sort_unstable();
        keys.dedup();

        // Fixture checks: a parser that silently extracts nothing (or loses the
        // values this test was written for) must fail LOUDLY, not pass
        // vacuously.
        assert!(
            keys.len() >= 10,
            "fixture check: expected at least the ten known summaries, found {keys:?}"
        );
        for expected in ["unencrypted", "no-key", "key-service-unavailable"] {
            assert!(
                keys.contains(&expected),
                "fixture check: a summary this test was written for is gone: {keys:?}"
            );
        }

        for key in keys {
            assert!(
                guide.contains(key),
                "KeyStatus summary {key:?} is emitted but not listed in \
                 USING_THE_ENGINE.md — a front-end localising only the \
                 documented values renders this one raw"
            );
        }
    }

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
    fn strict_gate_passes_an_unencrypted_disc() {
        assert!(ensure_decryptable_strict(&disc(false), false).is_ok());
    }

    #[test]
    fn strict_gate_passes_a_resolved_encrypted_disc() {
        let mut d = disc(true);
        d.aacs = Some(aacs(libfreemkv::KeyOrigin::KeyDb));
        assert!(ensure_decryptable_strict(&d, false).is_ok());
    }

    #[test]
    fn strict_gate_surfaces_a_key_source_failure() {
        // aacs: None + aacs_error: Some — the gap the strict gate exists to
        // close. Deleting its body lets this encrypted-but-unresolved disc pass
        // as decryptable; the gate must instead surface the key-service failure.
        let mut d = disc(true);
        d.aacs_error = Some(libfreemkv::Error::KeyServiceUnavailable);
        assert!(matches!(
            ensure_decryptable_strict(&d, false),
            Err(libfreemkv::Error::KeyServiceUnavailable)
        ));
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
