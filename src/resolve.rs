//! The executors' decrypt gate (KU §3.5): the rip's up-front key set decides, and nothing
//! else. Key status as data is [`crate::keys::key_status`] over that set.

use libfreemkv::Error;
use libfreemkv::keys::{KeyScope, ResolvedKeySet, check_decryptable};

// KU §8.2 KU-X1: "Remove the legacy gate fallback". With no set the gate reads the empty
// set, so an AACS disc refuses whatever keys it banked; CSS is read from the disc (coord 7).
pub(crate) fn ensure_decryptable_with(
    disc: &libfreemkv::Disc,
    raw: bool,
    keys: Option<&ResolvedKeySet>,
) -> crate::Result<()> {
    match keys {
        Some(set) => check_decryptable(disc, raw, Some(set), &KeyScope::WholeDisc),
        None => ensure_decryptable_without_a_set(disc, raw),
    }
}

// The library gate over the empty set, plus a refusal of an encrypted disc with neither AACS
// nor CSS state (`aacs: None` after a key-source failure), which the library gate passes.
fn ensure_decryptable_without_a_set(disc: &libfreemkv::Disc, raw: bool) -> crate::Result<()> {
    check_decryptable(
        disc,
        raw,
        Some(&ResolvedKeySet::none()),
        &KeyScope::WholeDisc,
    )?;
    if disc.encrypted && !raw && disc.css.is_none() {
        // A key SOURCE failure keeps its own verdict: "retry / fix the token", not "no key".
        return Err(match disc.aacs_error {
            Some(Error::KeyServiceUnavailable) => Error::KeyServiceUnavailable,
            Some(Error::KeyServiceUnauthorized) => Error::KeyServiceUnauthorized,
            Some(Error::KeyServiceRateLimited) => Error::KeyServiceRateLimited,
            _ => Error::NoDiscKey {
                disc_hash: disc
                    .aacs
                    .as_ref()
                    .map(|a| a.disc_hash.clone())
                    .unwrap_or_default(),
            },
        });
    }
    Ok(())
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

    fn aacs() -> libfreemkv::AacsState {
        libfreemkv::test_util::aacs_state().build()
    }

    #[test]
    fn gate_without_a_set_passes_an_unencrypted_disc_and_a_raw_copy() {
        assert!(ensure_decryptable_with(&disc(false), false, None).is_ok());
        let mut d = disc(true);
        d.aacs = Some(aacs());
        assert!(ensure_decryptable_with(&d, true, None).is_ok());
    }

    // KU §8.2 KU-X1: "Remove the legacy gate fallback": keys banked on the disc open nothing.
    #[test]
    fn gate_without_a_set_refuses_disc_banked_keys() {
        let mut d = disc(true);
        d.aacs = Some(aacs());
        let e = ensure_decryptable_with(&d, false, None).unwrap_err();
        assert_eq!(e.code(), libfreemkv::error::E_NO_DISC_KEY, "{e}");
    }

    #[test]
    fn gate_without_a_set_surfaces_a_key_source_failure() {
        // aacs: None + aacs_error: Some — the gap this gate exists to
        // close. Deleting its body lets this encrypted-but-unresolved disc pass
        // as decryptable; the gate must instead surface the key-service failure.
        let mut d = disc(true);
        d.aacs_error = Some(libfreemkv::Error::KeyServiceUnavailable);
        assert!(matches!(
            ensure_decryptable_with(&d, false, None),
            Err(libfreemkv::Error::KeyServiceUnavailable)
        ));
    }

    /// ...and each key-service failure keeps its OWN identity through the
    /// gate. Collapsing any of these arms into the `_` fallthrough returns
    /// `NoDiscKey` — "this disc has no key", the wrong operator action — when
    /// what actually happened is "the key service refused/throttled us".
    #[test]
    fn gate_without_a_set_surfaces_an_unauthorized_key_service_as_itself() {
        let mut d = disc(true);
        d.aacs_error = Some(libfreemkv::Error::KeyServiceUnauthorized);
        assert!(matches!(
            ensure_decryptable_with(&d, false, None),
            Err(libfreemkv::Error::KeyServiceUnauthorized)
        ));
    }

    #[test]
    fn gate_without_a_set_surfaces_a_rate_limited_key_service_as_itself() {
        let mut d = disc(true);
        d.aacs_error = Some(libfreemkv::Error::KeyServiceRateLimited);
        assert!(matches!(
            ensure_decryptable_with(&d, false, None),
            Err(libfreemkv::Error::KeyServiceRateLimited)
        ));
    }
}
