# Fuzz targets

Requires nightly Rust and `cargo-fuzz`:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run proto_decode -- -max_total_time=60
```

`proto_decode` asserts that arbitrary datagrams never panic and that a decoded
event re-encodes within `MAX_EVENT_SIZE`. The CI quality workflow runs a 60 s
smoke on manual dispatch.

Corpora/artifacts are intentionally not committed (see `.gitignore`); commit a
minimized crashing input under `fuzz/corpus/proto_decode/` when a real bug is
found, together with its regression test.
