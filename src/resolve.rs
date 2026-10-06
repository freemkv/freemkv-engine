//! The executors' decrypt gate (KU §3.5): the rip's up-front key set decides, and nothing
//! else. Key status as data is [`crate::keys::key_status`] over that set.

use libfreemkv::Error;
use libfreemkv::keys::{KeyRing, KeyScope, check_decryptable};

// KU §8.2 KU-X1: "Remove the legacy gate fallback". With no set the gate reads the empty
// set, so an AACS disc refuses whatever keys it banked; CSS is read from the disc (coord 7).
pub(crate) fn ensure_decryptable_with(
    disc: &libfreemkv::Disc,
    raw: bool,
    keys: Option<&KeyRing>,
) -> crate::Result<()> {
    match keys {
        Some(set) => check_decryptable(disc, raw, Some(set), &KeyScope::WholeDisc),
        None => ensure_decryptable_without_a_set(disc, raw),
    }
}

// The library gate over the empty set, plus a refusal of an encrypted disc with neither AACS
// nor CSS state (`aacs: None` after a key-source failure), which the library gate passes.
fn ensure_decryptable_without_a_set(disc: &libfreemkv::Disc, raw: bool) -> crate::Result<()> {
    check_decryptable(disc, raw, Some(&KeyRing::none()), &KeyScope::WholeDisc)?;
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
#[path = "resolve_tests.rs"]
mod tests;
