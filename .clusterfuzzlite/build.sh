#!/bin/bash -eu
# Build the fuzz targets in fuzz/ and hand them to ClusterFuzzLite. The builder image's nightly
# is older than the app's minimum Rust version: use a recent, fixed nightly instead.
NIGHTLY=nightly-2026-10-14
rustup toolchain install --profile minimal "$NIGHTLY"
cd "$SRC/recouvrance"
cargo "+$NIGHTLY" fuzz build -O
for target in filter quantity yaml; do
  cp "fuzz/target/x86_64-unknown-linux-gnu/release/$target" "$OUT/"
done
