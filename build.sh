#!/bin/sh
# Keenetic (MIPS big-endian, soft-float) için statik zhub ikilisi üretir.
# Kullanım: ./build.sh  → target/mips-unknown-linux-musl/release/zhub
set -e
cd "$(dirname "$0")"
docker build -q -t zhub-mips-builder docker >/dev/null
docker run --rm -v "$PWD":/src -w /src \
    -v zhub-cargo-registry:/usr/local/cargo/registry \
    -v zhub-cargo-git:/usr/local/cargo/git \
    zhub-mips-builder sh -c '
cargo +nightly test
RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=no" \
    cargo +nightly zigbuild --release --target mips-unknown-linux-musl -Z build-std=std,panic_abort'
ls -l target/mips-unknown-linux-musl/release/zhub
