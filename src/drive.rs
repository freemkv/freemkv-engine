//! The front ends' one door to a drive device. Work that reads a disc (a rip, a copy, a
//! title mux) goes through [`crate::run()`]; what is left for a front end is the device
//! itself: open it to eject, to show what it is, to probe it before a scan, or to bring it
//! back after a transport fault.

use std::path::Path;

/// Open the drive at `path` (no bring-up, no scan).
pub fn open(path: &Path) -> crate::Result<libfreemkv::Drive> {
    libfreemkv::Drive::open(path)
}

/// Open and bring up the drive `target` as a session under `spec`'s credentials, ready to
/// scan (no scan yet).
pub fn open_session(
    target: libfreemkv::DeviceTarget,
    spec: libfreemkv::KeySpec,
) -> crate::Result<libfreemkv::DiscSession> {
    libfreemkv::DiscSession::open(target, spec)
}
