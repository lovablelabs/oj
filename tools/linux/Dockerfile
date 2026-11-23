# The Linux box tools/linux.sh runs: CI's toolchain (Node 22, the pinned Rust,
# clang for bindgen) plus Bun and Chromium's system libraries.
FROM node:22-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang libclang-dev cmake pkg-config ca-certificates curl unzip git procps psmisc time \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_TOOLCHAIN
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path \
        --profile minimal --default-toolchain "$RUST_TOOLCHAIN" --component rustfmt,clippy

ARG PLAYWRIGHT_VERSION
RUN npx -y "playwright@$PLAYWRIGHT_VERSION" install-deps chromium \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g bun
