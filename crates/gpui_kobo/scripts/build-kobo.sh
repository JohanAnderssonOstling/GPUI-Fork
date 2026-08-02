#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../../.." && pwd)
TARGET=armv7-unknown-linux-musleabihf

TOOLCHAIN_NAME=armv7l-linux-musleabihf-cross
TOOLCHAIN_URL=https://musl.cc/armv7l-linux-musleabihf-cross.tgz
TOOLCHAIN_SHA256=f49f1a15ec62364ef5e4edb4e3990c0e1d2d1a54c90153b8f3869dad63328a10
BUILD_ROOT=${KOBO_BUILD_ROOT:-"$REPO_ROOT/target/gpui-kobo"}
TOOLCHAIN_CACHE=${KOBO_TOOLCHAIN_CACHE:-"$BUILD_ROOT/toolchain"}
TOOLCHAIN_ARCHIVE=${KOBO_TOOLCHAIN_ARCHIVE:-"$TOOLCHAIN_CACHE/$TOOLCHAIN_NAME.tgz"}
TOOLCHAIN_ROOT="$TOOLCHAIN_CACHE/$TOOLCHAIN_NAME"

checksum() {
    sha256sum "$1" | cut -d ' ' -f 1
}

mkdir -p "$TOOLCHAIN_CACHE"
if [ ! -f "$TOOLCHAIN_ARCHIVE" ]; then
    temporary="$TOOLCHAIN_ARCHIVE.partial"
    curl -fL --retry 3 "$TOOLCHAIN_URL" -o "$temporary"
    mv "$temporary" "$TOOLCHAIN_ARCHIVE"
fi

actual=$(checksum "$TOOLCHAIN_ARCHIVE")
if [ "$actual" != "$TOOLCHAIN_SHA256" ]; then
    printf 'Toolchain checksum mismatch: expected %s, got %s\n' \
        "$TOOLCHAIN_SHA256" "$actual" >&2
    exit 1
fi

if [ ! -x "$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-gcc" ]; then
    rm -rf "$TOOLCHAIN_ROOT"
    tar -xzf "$TOOLCHAIN_ARCHIVE" -C "$TOOLCHAIN_CACHE"
fi

CC="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-gcc"
AR="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-ar"
CXX="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-g++"

if ! rustup target list --installed | grep -qx "$TARGET"; then
    rustup target add "$TARGET"
fi

export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$CC"
export CC_armv7_unknown_linux_musleabihf="$CC"
export CXX_armv7_unknown_linux_musleabihf="$CXX"
export AR_armv7_unknown_linux_musleabihf="$AR"

cd "$REPO_ROOT"
cargo build \
    --release \
    --target "$TARGET" \
    -p gpui_kobo \
    --bin gpui-kobo-button

binary="$REPO_ROOT/target/$TARGET/release/gpui-kobo-button"
if [ ! -x "$binary" ]; then
    printf 'Cargo completed without producing %s\n' "$binary" >&2
    exit 1
fi
printf '%s\n' "$binary"
