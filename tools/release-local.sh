#!/usr/bin/env bash
# Builds the Linux and Windows release packages on a Linux x86_64 machine, the way the release
# workflow does, into dist/ (gitignored). macOS is built only on a macOS runner (lipo, SDK).
#
#   tools/release-local.sh                 # linux + windows
#   tools/release-local.sh linux           # the four static musl targets, .deb and .rpm
#   tools/release-local.sh windows         # x86_64-pc-windows-msvc with the static CRT
#   NO_PACKAGES=1 tools/release-local.sh   # archives only, no .deb / .rpm
#   LINUX_TARGETS=x86_64-unknown-linux-musl tools/release-local.sh linux   # some targets only
#
# The Dockerfile runs `linux` in a container with these tools, for a host without them.
#
# Needs (docs/release.md has the details):
#   linux:   zig 0.16 on PATH (or ZIG_DIR=<dir with zig>), cargo-zigbuild, readelf; for packages
#            cargo-deb and cargo-generate-rpm; rustup targets x86_64/aarch64-unknown-linux-musl,
#            armv7-unknown-linux-musleabihf, arm-unknown-linux-musleabihf
#   windows: cargo-xwin, clang/lld/llvm-lib (LLVM), rustup target x86_64-pc-windows-msvc
#
# Builds go to $CARGO_TARGET_DIR (or target/). Never /tmp: point TMPDIR at a disk if yours is
# a tmpfs.
set -euo pipefail
cd "$(dirname "$0")/.."

LINUX_TARGETS=${LINUX_TARGETS:-"x86_64-unknown-linux-musl aarch64-unknown-linux-musl armv7-unknown-linux-musleabihf arm-unknown-linux-musleabihf"}
WINDOWS_TARGET=x86_64-pc-windows-msvc
TARGET_DIR=${CARGO_TARGET_DIR:-target}

want_linux=0
want_windows=0
[ $# -eq 0 ] && set -- linux windows
for a in "$@"; do
    case "$a" in
    linux) want_linux=1 ;;
    windows) want_windows=1 ;;
    *) sed -n '2,19p' "$0" >&2; exit 2 ;;
    esac
done

need() {
    command -v "$1" >/dev/null || {
        echo "release-local: $1 is needed ($2)" >&2
        exit 1
    }
}

version=$(tools/dist.sh version)
echo "release-local: swarmy $version"
# A fresh dist/: SHA256SUMS covers what this run built, nothing older.
rm -rf dist
mkdir -p dist

if [ "$want_linux" = 1 ]; then
    if ! command -v zig >/dev/null && [ -n "${ZIG_DIR:-}" ]; then
        PATH="$ZIG_DIR:$PATH"
    fi
    need zig "zig 0.16: https://ziglang.org/download/"
    need cargo-zigbuild "cargo install --locked cargo-zigbuild"
    for t in $LINUX_TARGETS; do
        echo "release-local: building $t"
        cargo zigbuild --locked --release --target "$t"
        tools/dist.sh check-static "$TARGET_DIR/$t/release/swarmy"
    done
    # Man page and completions come from the binary that runs here (an x86_64 or an aarch64
    # machine); they are the same for every target.
    host="$(uname -m)-unknown-linux-musl"
    [ -x "$TARGET_DIR/$host/release/swarmy" ] || {
        echo "release-local: the man page needs the $host build: add it to LINUX_TARGETS" >&2
        exit 1
    }
    tools/dist.sh docs "$TARGET_DIR/$host/release/swarmy"
    for t in $LINUX_TARGETS; do
        tools/dist.sh archive "$t" "$TARGET_DIR/$t/release/swarmy"
    done
    if [ "${NO_PACKAGES:-0}" != 1 ]; then
        # One armhf .deb: the ARMv6 build, which runs on every 32-bit Raspberry Pi (Raspberry Pi
        # OS's armhf is ARMv6) and on any armhf Debian or Ubuntu (ARMv7).
        # Packages of the targets built in this run.
        built() { case " $LINUX_TARGETS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
        if built x86_64-unknown-linux-musl; then tools/dist.sh deb x86_64-unknown-linux-musl; fi
        if built aarch64-unknown-linux-musl; then tools/dist.sh deb aarch64-unknown-linux-musl; fi
        if built arm-unknown-linux-musleabihf; then tools/dist.sh deb arm-unknown-linux-musleabihf; fi
        if built x86_64-unknown-linux-musl; then tools/dist.sh rpm x86_64-unknown-linux-musl x86_64; fi
        if built aarch64-unknown-linux-musl; then tools/dist.sh rpm aarch64-unknown-linux-musl aarch64; fi
        if built armv7-unknown-linux-musleabihf; then tools/dist.sh rpm armv7-unknown-linux-musleabihf armv7hl; fi
    fi
fi

if [ "$want_windows" = 1 ]; then
    need cargo-xwin "cargo install --locked cargo-xwin"
    # cargo-xwin needs llvm-lib and lld-link; Debian and Ubuntu keep them in /usr/lib/llvm-*/bin.
    if ! command -v llvm-lib >/dev/null; then
        for d in $(printf '%s\n' /usr/lib/llvm-*/bin | sort -V -r); do
            if [ -x "$d/llvm-lib" ]; then
                PATH="$d:$PATH"
                break
            fi
        done
    fi
    need llvm-lib "LLVM (clang, lld, llvm-lib)"
    echo "release-local: building $WINDOWS_TARGET"
    # The static CRT comes from .cargo/config.toml (+crt-static).
    cargo xwin build --locked --release --target "$WINDOWS_TARGET"
    exe="$TARGET_DIR/$WINDOWS_TARGET/release/swarmy.exe"
    if command -v llvm-readobj >/dev/null; then
        dlls=$(llvm-readobj --coff-imports "$exe" | sed -n 's/^ *Name: //p' | sort -u | tr '\n' ' ')
        echo "release-local: swarmy.exe imports $dlls"
        case "$dlls" in
        *VCRUNTIME* | *vcruntime* | *api-ms-win-crt* | *MSVCP*)
            echo "release-local: swarmy.exe needs the Visual C++ runtime: the CRT is not static" >&2
            exit 1
            ;;
        esac
    fi
    if [ "$want_linux" != 1 ]; then
        # Docs from a host build when Linux was not built in this run.
        cargo build --locked --release
        tools/dist.sh docs "$TARGET_DIR/release/swarmy"
    fi
    tools/dist.sh archive "$WINDOWS_TARGET" "$exe"
fi

tools/dist.sh sums
ls -la dist
