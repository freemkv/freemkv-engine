[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![CI](https://github.com/freemkv/freemkv-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/freemkv/freemkv-engine/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/freemkv/freemkv-engine/branch/dev/graph/badge.svg)](https://codecov.io/gh/freemkv/freemkv-engine)

# freemkv-engine

The freemkv rip engine: disc-to-MKV orchestration on top of
[libfreemkv](https://github.com/freemkv/libfreemkv), shared by every front-end
(CLI, autorip, desktop UI).

libfreemkv handles drive access, parsing, decryption, muxing and single-shot
reads. This crate adds the pieces above that: title selection, preflight,
sweep and patch retries with mapfiles, and job execution. A front-end supplies
a `Job` and a `Sink`, and diagnostics and progress flow back through the sink.

It is not published on crates.io. Depend on it by git tag, like the other
freemkv crates. Requires Rust 1.88 or later (`rust-version` in `Cargo.toml`).

See [USING_THE_ENGINE.md](USING_THE_ENGINE.md) for integration details and the
required `[patch.crates-io]` block, [CHANGELOG.md](CHANGELOG.md) for release
notes, and [CONTRIBUTING.md](CONTRIBUTING.md) before sending changes.

## License

MIT. See [LICENSE](LICENSE).
