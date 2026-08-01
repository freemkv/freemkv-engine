//! Every type a consumer can OBTAIN must also be NAMEABLE.
//!
//! A `pub` type inside a private module that is not re-exported can be bound by
//! inference but can never be written in a signature, a struct field, or a
//! turbofish. `RipMode` was the sharp case: `Job.mode` and `Job::with_mode` are
//! the documented way to ask for a multipass rip, and no consumer could name a
//! variant to pass, so `RipMode::Multi` was unreachable through the public API.
//!
//! These are compile-time assertions. If an export is dropped this file stops
//! compiling, which is the point — a doc test or a runtime check could not
//! catch it.

/// The multipass mode selector, and the builder that takes it.
#[test]
fn rip_mode_is_nameable_and_settable() {
    let mode: freemkv_engine::RipMode = freemkv_engine::RipMode::Multi;
    let job = freemkv_engine::Job::new("iso://in.iso", "mkv://out.mkv").with_mode(mode);
    assert!(matches!(job.mode, freemkv_engine::RipMode::Multi));

    // The field is settable directly too — the other half of the API.
    let mut j2 = freemkv_engine::Job::new("iso://in.iso", "mkv://out.mkv");
    j2.mode = freemkv_engine::RipMode::Single;
    assert!(matches!(j2.mode, freemkv_engine::RipMode::Single));
}

/// Types returned by exported functions must be nameable, or a front-end
/// cannot store the value it was just handed.
#[test]
fn returned_types_are_nameable() {
    fn _holds_key_status(_: freemkv_engine::KeyStatus) {}
    fn _holds_multipass_result(_: freemkv_engine::MultipassResult) {}
    fn _holds_pass_plan(_: freemkv_engine::PassPlan) {}
    fn _holds_rip_file(_: freemkv_engine::RipFile) {}
    fn _holds_unmatched(_: Vec<freemkv_engine::UnmatchedClass>) {}
    // `Preflight::Blocked` carries these; a UI must destructure it.
    fn _holds_reasons(_: &[freemkv_engine::Reason]) {}
}

/// `resolve_stream_selection` is referenced by `StreamFilter`'s own doc as
/// `[crate::resolve_stream_selection]` and by the CHANGELOG, so the intra-doc
/// link was pointing at something no consumer could call.
#[test]
fn documented_free_functions_are_callable() {
    let _: fn(
        &libfreemkv::DiscTitle,
        &freemkv_engine::StreamFilter,
        &freemkv_engine::StreamFilter,
    ) -> Result<libfreemkv::StreamSelection, freemkv_engine::StreamSelError> =
        freemkv_engine::resolve_stream_selection;
}
