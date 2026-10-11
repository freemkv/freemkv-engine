//! The executors' decrypt gate (KU §3.5): the rip's up-front key set decides, and nothing
//! else. Key status as data is [`crate::keys::key_status`] over that set.

use libfreemkv::keys::{KeyRing, KeyScope, check_decryptable};

// KU §8.2 KU-X1: "Remove the legacy gate fallback". With no set the gate reads the empty
// set, so an AACS disc refuses whatever keys it banked; CSS is read from the disc (coord 7).
pub(crate) fn ensure_decryptable_with(
    disc: &libfreemkv::Disc,
    raw: bool,
    keys: Option<&KeyRing>,
) -> crate::Result<()> {
    let none = KeyRing::none();
    check_decryptable(disc, raw, Some(keys.unwrap_or(&none)), &KeyScope::WholeDisc)
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
