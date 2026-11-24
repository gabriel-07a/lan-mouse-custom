# Fuzz targets

Requires nightly Rust and `cargo-fuzz`:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run proto_decode -- -max_total_time=60
cargo +nightly fuzz run clipboard_transfer -- -max_total_time=60
```

Without nightly, stable can build the same instrumented binaries (no
AddressSanitizer, which safe Rust rarely needs):

```sh
cd fuzz
RUSTFLAGS="-Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=4 \
  -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
  -Cllvm-args=-sanitizer-coverage-pc-table \
  -Cllvm-args=-sanitizer-coverage-trace-compares \
  --cfg fuzzing -Cdebug-assertions -Coverflow-checks" \
  cargo build --release --bins --target x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/clipboard_transfer -max_total_time=60
rm -rf target
```

(`--target` keeps the instrumentation flags off build scripts.)

- `proto_decode`: arbitrary datagrams never panic, and a decoded event
  re-encodes within `MAX_EVENT_SIZE`.
- `clipboard_transfer`: a peer's offer and chunks (any bytes) go through
  `ClipboardFrame::decode` and `IncomingTransfer` without panicking or
  allocating past the limits; every decoded frame re-encodes to an equal frame
  that fits `MAX_CLIPBOARD_FRAME`.

The CI quality workflow fuzzes each target for 60 s weekly and on manual
dispatch.

Corpora/artifacts are intentionally not committed (see `.gitignore`); commit a
minimized crashing input under `fuzz/corpus/proto_decode/` when a real bug is
found, together with its regression test.
