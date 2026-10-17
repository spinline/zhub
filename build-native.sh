#!/bin/sh
# Builds a static zhub binary for Keenetic (MIPS big-endian, soft-float) without Docker:
# the host's Rust nightly cross-compiles and zig links (musl). Needed for the Matter
# build: rs-matter does not fit Docker Desktop's default VM memory, while natively it
# compiles in about 5 GB.
# Usage: ./build-native.sh [cargo options, e.g. --features matter]
#        → target/mips-unknown-linux-musl/release/zhub
set -e
cd "$(dirname "$0")"
TOOLCHAIN=nightly-2026-10-16
TOOLS="$PWD/.tools"

if ! rustup toolchain list | grep -q "^$TOOLCHAIN"; then
    rustup toolchain install "$TOOLCHAIN" --profile minimal --component rust-src
fi
if [ ! -x "$TOOLS/zigenv/bin/python" ]; then
    # zig 0.16 fails to link musl for MIPS; 0.15.2 works
    python3 -m venv "$TOOLS/zigenv"
    "$TOOLS/zigenv/bin/pip" install -q ziglang==0.15.2
fi
cat > "$TOOLS/zigcc-mips.sh" <<WRAPPER
#!/bin/sh
exec "$TOOLS/zigenv/bin/python" -m ziglang cc -target mips-linux-musleabi -mcpu=mips32r2+soft_float "\$@"
WRAPPER
chmod +x "$TOOLS/zigcc-mips.sh"

cargo +"$TOOLCHAIN" test "$@"
CARGO_TARGET_MIPS_UNKNOWN_LINUX_MUSL_LINKER="$TOOLS/zigcc-mips.sh" \
RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=no" \
    cargo +"$TOOLCHAIN" build --release --target mips-unknown-linux-musl -Z build-std=std,panic_abort "$@"
ls -l target/mips-unknown-linux-musl/release/zhub
