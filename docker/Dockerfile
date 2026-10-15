FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends python3-pip file qemu-user && rm -rf /var/lib/apt/lists/*
RUN pip3 install --break-system-packages "ziglang==0.15.2" cargo-zigbuild
RUN rustup toolchain install nightly --profile minimal --component rust-src
