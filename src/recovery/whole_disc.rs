//! The whole-disc (sweep / patch) decrypting reader. The rules (every content file keyed,
//! E7032 refusals, each file's own unit grid) live in [`libfreemkv::whole_disc`], shared
//! with freemkv's image → ISO path so the GUI and CLI never diverge.

use libfreemkv::error::Result;
use libfreemkv::sector::SectorSource;

/// The reader sweep and patch read through.
pub(crate) type WholeDiscReader<'r> =
    libfreemkv::whole_disc::WholeDiscReader<&'r mut dyn SectorSource>;

/// Whole-disc reader. Decrypting with the rip's key set (KU §3.2), the set's reader: its
/// keyed pieces, and the on-arrival proof for the rest, with no lookup. Otherwise (until
/// KU-X1) `decrypt` installs the disc-banked keys; `--raw` / CSS / clear discs pass through.
pub(crate) fn whole_disc_decrypting_reader<'r>(
    disc: &libfreemkv::Disc,
    reader: &'r mut dyn SectorSource,
    decrypt: bool,
    halt: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    key_fetch: Option<&libfreemkv::sector::KeyFetch>,
    keys: Option<&libfreemkv::keys::ResolvedKeySet>,
) -> Result<WholeDiscReader<'r>> {
    let halt = halt.cloned().map(libfreemkv::halt::Halt::from_arc);
    match keys {
        Some(set) if decrypt => set.whole_disc_reader(disc, reader, halt.as_ref()),
        _ => libfreemkv::whole_disc::whole_disc_reader(
            disc,
            reader,
            decrypt,
            key_fetch,
            halt.as_ref(),
        ),
    }
}
