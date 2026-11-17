#![no_main]
use freemkv_engine::{SectorStatus, parse_error_code};
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if data.len() > 65536 {
        return;
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_error_code(text);
        for c in text.chars() {
            if let Some(status) = SectorStatus::from_char(c) {
                assert_eq!(status.to_char(), c);
            }
        }
    }
});
