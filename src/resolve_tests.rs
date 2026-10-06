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
