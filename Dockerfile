# syntax=docker/dockerfile:1
#
# Builds swarmy's Linux release packages without Rust on the host: the four static musl binaries
# (x86_64, aarch64, ARMv7, ARMv6), their archives, .deb and .rpm, as tools/release-local.sh
# makes them, into dist/. The binaries are static: run them on the host, not in a container
# (swarmy reads the host's agents, processes and terminals).
#
#   docker build --output dist .                                     # everything, into ./dist
#   docker build --build-arg TARGETS=x86_64-unknown-linux-musl \
#                --build-arg NO_PACKAGES=1 --output dist .           # one binary, no packages
#
# The `toolchain` stage is the build environment alone, for a shell or a command on the sources
# mounted from the host (builds land in the mounted target/ and stay incremental):
#
#   docker build --target toolchain -t swarmy-toolchain .
#   docker run --rm -it -u "$(id -u):$(id -g)" -v "$PWD:/src" swarmy-toolchain tools/check.sh
#
# The versions below are the ones CI and the release use (.github/workflows): change them here.

ARG RUST_VERSION=1.99.1

FROM rust:${RUST_VERSION}-slim-trixie AS toolchain
ARG ZIG_VERSION=0.16.0
ARG ZIG_SHA256_X86_64=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00
ARG ZIG_SHA256_AARCH64=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17
ARG CARGO_ZIGBUILD_VERSION=0.24.0
ARG CARGO_DEB_VERSION=3.8.0
ARG CARGO_GENERATE_RPM_VERSION=0.21.1
# readelf (the static check), xz (zig's tarball), git (cargo install --git, tags).
RUN apt-get update \
    && apt-get install -y --no-install-recommends binutils ca-certificates curl git xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    arch=$(uname -m); \
    case "$arch" in \
      x86_64) sum=$ZIG_SHA256_X86_64 ;; \
      aarch64) sum=$ZIG_SHA256_AARCH64 ;; \
      *) echo "no zig checksum for $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSLo /tmp/zig.tar.xz "https://ziglang.org/download/$ZIG_VERSION/zig-$arch-linux-$ZIG_VERSION.tar.xz"; \
    echo "$sum  /tmp/zig.tar.xz" | sha256sum -c -; \
    mkdir -p /opt/zig; \
    tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    rm /tmp/zig.tar.xz; \
    ln -s /opt/zig/zig /usr/local/bin/zig
RUN rustup component add rustfmt clippy \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
       armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf
RUN cargo install --locked \
      "cargo-zigbuild@$CARGO_ZIGBUILD_VERSION" \
      "cargo-deb@$CARGO_DEB_VERSION" \
      "cargo-generate-rpm@$CARGO_GENERATE_RPM_VERSION" \
    && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"
# Runnable as the host's user (-u): cargo writes its registry under CARGO_HOME.
RUN chmod -R a+rwX "$CARGO_HOME" "$RUSTUP_HOME"
ENV HOME=/tmp
WORKDIR /src

FROM toolchain AS build
ARG TARGETS=""
ARG NO_PACKAGES=0
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eu; \
    if [ -n "$TARGETS" ]; then export LINUX_TARGETS="$TARGETS"; fi; \
    NO_PACKAGES="$NO_PACKAGES" tools/release-local.sh linux; \
    rm -rf dist/.stage; \
    mkdir -p /out; \
    cp -r dist/. /out/

FROM scratch AS dist
COPY --from=build /out/ /
