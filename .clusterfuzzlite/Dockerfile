# The OSS-Fuzz Rust builder (nightly Rust and cargo-fuzz), pinned by digest.
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f
COPY . $SRC/recouvrance
WORKDIR $SRC/recouvrance
COPY .clusterfuzzlite/build.sh $SRC/
