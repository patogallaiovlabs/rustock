# syntax=docker/dockerfile:1
#
# rustock-cli on Ubuntu 22.04 (glibc 2.35), for linux/amd64 and linux/arm64:
#
#   docker build -t rustock .
#   docker buildx build --platform linux/amd64,linux/arm64 -t rustock .
#
# The binary is at /usr/local/bin/rustock-cli in the runtime image. There is
# no entrypoint, so the image runs any command (`docker run rustock rustock-cli --help`).
# To use the binary outside Docker, copy it out of a container:
#
#   docker create --name rustock-bin rustock && docker cp rustock-bin:/usr/local/bin/rustock-cli . && docker rm rustock-bin

FROM ubuntu:22.04 AS build

# Rust toolchain installed with rustup, pinned. Override with --build-arg.
ARG RUST_VERSION=1.92.0

ENV DEBIAN_FRONTEND=noninteractive
# RocksDB (librocksdb-sys) builds from source with a C++ compiler and needs
# libclang for its bindings.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential clang libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal --default-toolchain "${RUST_VERSION}" \
    && rustc --version

WORKDIR /src
COPY . .
RUN cargo build --release --locked -p rustock-cli \
    && cp target/release/rustock-cli /usr/local/bin/rustock-cli \
    && /usr/local/bin/rustock-cli --version

FROM ubuntu:22.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/rustock-cli /usr/local/bin/rustock-cli
CMD ["rustock-cli", "--help"]
