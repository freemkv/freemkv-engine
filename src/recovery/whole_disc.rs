//! The whole-disc (sweep / patch) decrypting reader. The rules (every content file keyed,
//! E7032 refusals, each file's own unit grid) live in [`libfreemkv::whole_disc`], shared
//! with freemkv's image → ISO path so the GUI and CLI never diverge.

use libfreemkv::error::Result;
use libfreemkv::sector::SectorSource;

/// The reader sweep and patch read through.
pub(crate) type WholeDiscReader<'r> =
    libfreemkv::whole_disc::WholeDiscReader<&'r mut dyn SectorSource>;

/// Whole-disc reader. `--raw`: the raw reader, which never decrypts. Decrypting with the
/// rip's key set (KU §3.2), the set's reader: its keyed pieces, and the on-arrival proof
/// for the rest, with no lookup. Decrypting without one: CSS / clear discs through an empty
/// set; the decrypt gate has already refused an AACS disc (KU-X1).
pub(crate) fn whole_disc_decrypting_reader<'r>(
    disc: &libfreemkv::Disc,
    reader: &'r mut dyn SectorSource,
    decrypt: bool,
    halt: Option<&libfreemkv::halt::Halt>,
    keys: Option<&libfreemkv::keys::ResolvedKeySet>,
) -> Result<WholeDiscReader<'r>> {
    if !decrypt {
        return Ok(libfreemkv::whole_disc::raw_whole_disc_reader(reader));
    }
    match keys {
        Some(set) => set.whole_disc_reader(disc, reader, halt),
        // No halt: like the pre-KU-X2 reader, a stop is the pass's to observe (it ends halted,
        // not Err(Halted) at construction); the empty set's non-AACS arm reads it nowhere else.
        None => libfreemkv::keys::ResolvedKeySet::none().whole_disc_reader(disc, reader, None),
    }
}
