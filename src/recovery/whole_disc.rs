//! The whole-disc (sweep / patch) decrypting reader. The rules (every content file keyed,
//! E7032 refusals, each file's own unit grid) live in [`libfreemkv::whole_disc`], shared
//! with freemkv's image → ISO path so the GUI and CLI never diverge.

use libfreemkv::error::Result;
use libfreemkv::sector::SectorSource;

/// The reader sweep and patch read through.
pub(crate) type WholeDiscReader<'r> =
    libfreemkv::whole_disc::WholeDiscReader<&'r mut dyn SectorSource>;

/// Whole-disc reader. Decrypting with the rip's key set (KU §3.2), the set's reader: its
/// keyed pieces, and the on-arrival proof for the rest, with no lookup. Without one: `--raw`
/// / CSS / clear discs; the decrypt gate has already refused an AACS disc (KU-X1).
pub(crate) fn whole_disc_decrypting_reader<'r>(
    disc: &libfreemkv::Disc,
    reader: &'r mut dyn SectorSource,
    decrypt: bool,
    halt: Option<&libfreemkv::halt::Halt>,
    keys: Option<&libfreemkv::keys::ResolvedKeySet>,
) -> Result<WholeDiscReader<'r>> {
    match keys {
        Some(set) if decrypt => set.whole_disc_reader(disc, reader, halt),
        _ => libfreemkv::whole_disc::whole_disc_reader(disc, reader, decrypt, None, halt),
    }
}
