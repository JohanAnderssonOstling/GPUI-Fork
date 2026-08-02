#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CRATE_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(CDPATH= cd -- "$CRATE_DIR/../.." && pwd)

FBINK_VERSION=v1.25.0
FBINK_ARCHIVE_NAME=FBInk-v1.25.0.tar.xz
FBINK_URL="https://github.com/NiLuJe/FBInk/releases/download/$FBINK_VERSION/$FBINK_ARCHIVE_NAME"
FBINK_SHA256=d598e99ed20994e08e1bb768512bc55c97cee941ffe6b1ac237aa754690e1ed5
BUILD_ROOT=${KOBO_BUILD_ROOT:-"$REPO_ROOT/target/gpui-kobo"}
TOOLCHAIN_CACHE=${KOBO_TOOLCHAIN_CACHE:-"$BUILD_ROOT/toolchain"}
TOOLCHAIN_ROOT="$TOOLCHAIN_CACHE/armv7l-linux-musleabihf-cross"
FBINK_ARCHIVE=${FBINK_SOURCE_ARCHIVE:-"$BUILD_ROOT/$FBINK_ARCHIVE_NAME"}
FBINK_BUILD_ROOT="$BUILD_ROOT/fbink-build"
FBINK_SOURCE_ROOT="$FBINK_BUILD_ROOT/FBInk-v1.25.0"
FBINK_BINARY="$FBINK_SOURCE_ROOT/Release/fbink"
FBINK_BUILD_MARKER="$FBINK_BUILD_ROOT/build-config"
FBINK_BUILD_KEY='v1.25.0 static-musl MINIMAL=1 BITMAP=1 IMAGE=1'
PACKAGE_ROOT="$BUILD_ROOT/package"
ARCHIVE="$BUILD_ROOT/gpui-kobo-test-armv7.tar.gz"

checksum() {
    sha256sum "$1" | cut -d ' ' -f 1
}

mkdir -p "$BUILD_ROOT"

KOBO_PREPARE_ONLY=1 "$SCRIPT_DIR/build-kobo.sh"

if [ ! -f "$FBINK_ARCHIVE" ]; then
    temporary="$FBINK_ARCHIVE.partial"
    curl -fL --retry 3 "$FBINK_URL" -o "$temporary"
    mv "$temporary" "$FBINK_ARCHIVE"
fi

actual=$(checksum "$FBINK_ARCHIVE")
if [ "$actual" != "$FBINK_SHA256" ]; then
    printf 'FBInk checksum mismatch: expected %s, got %s\n' \
        "$FBINK_SHA256" "$actual" >&2
    exit 1
fi

if [ ! -x "$FBINK_BINARY" ] || [ ! -f "$FBINK_BUILD_MARKER" ] || \
    [ "$(cat "$FBINK_BUILD_MARKER" 2>/dev/null)" != "$FBINK_BUILD_KEY" ]; then
    rm -rf "$FBINK_BUILD_ROOT"
    mkdir -p "$FBINK_BUILD_ROOT"
    tar -xJf "$FBINK_ARCHIVE" -C "$FBINK_BUILD_ROOT"
    jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1\n')
    make -C "$FBINK_SOURCE_ROOT" \
        -j "$jobs" \
        static \
        MINIMAL=1 \
        BITMAP=1 \
        IMAGE=1 \
        LDFLAGS=-static \
        CROSS_TC="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf"
    printf '%s\n' "$FBINK_BUILD_KEY" > "$FBINK_BUILD_MARKER"
fi

if [ ! -x "$FBINK_BINARY" ]; then
    printf 'FBInk build did not produce %s\n' "$FBINK_BINARY" >&2
    exit 1
fi

SHIM_OBJECT="$FBINK_SOURCE_ROOT/Release/gpui_fbink_shim.o"
SHIM_LIBRARY="$FBINK_SOURCE_ROOT/Release/libgpui_fbink_shim.a"
CC="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-gcc"
AR="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-ar"
"$CC" -Os -std=c11 -I"$FBINK_SOURCE_ROOT" \
    -c "$CRATE_DIR/native/fbink_shim.c" -o "$SHIM_OBJECT"
"$AR" rcs "$SHIM_LIBRARY" "$SHIM_OBJECT"

GPUI_BINARY=$(FBINK_LIB_DIR="$FBINK_SOURCE_ROOT/Release" "$SCRIPT_DIR/build-kobo.sh")

rm -rf "$PACKAGE_ROOT"
mkdir -p \
    "$PACKAGE_ROOT/.adds/gpui-kobo/third-party-source" \
    "$PACKAGE_ROOT/.adds/nm"
STRIP="$TOOLCHAIN_ROOT/bin/armv7l-linux-musleabihf-strip"
"$STRIP" -o "$PACKAGE_ROOT/.adds/gpui-kobo/gpui-kobo-button" "$GPUI_BINARY"
"$STRIP" -o "$PACKAGE_ROOT/.adds/gpui-kobo/fbink" "$FBINK_BINARY"
cp "$CRATE_DIR/kobo/run.sh" "$PACKAGE_ROOT/.adds/gpui-kobo/run.sh"
cp "$CRATE_DIR/kobo/nickelmenu" "$PACKAGE_ROOT/.adds/nm/gpui-kobo"
cp "$REPO_ROOT/LICENSE-APACHE" "$PACKAGE_ROOT/.adds/gpui-kobo/LICENSE-APACHE"
cp "$REPO_ROOT/assets/fonts/lilex/OFL.txt" \
    "$PACKAGE_ROOT/.adds/gpui-kobo/LICENSE-LILEX-OFL.txt"
cp "$FBINK_SOURCE_ROOT/LICENSE" "$PACKAGE_ROOT/.adds/gpui-kobo/LICENSE-FBINK-GPLv3"
cp "$FBINK_ARCHIVE" \
    "$PACKAGE_ROOT/.adds/gpui-kobo/third-party-source/$FBINK_ARCHIVE_NAME"
cat > "$PACKAGE_ROOT/.adds/gpui-kobo/BUILD-INFO.txt" <<EOF
gpui-kobo-button target: armv7-unknown-linux-musleabihf production Platform backend
GPUI fork commit: $(git -C "$REPO_ROOT" rev-parse HEAD)
FBInk version: $FBINK_VERSION
FBInk source SHA-256: $FBINK_SHA256
FBInk build: statically linked libfbink v1.25.0 plus C ABI shim; CLI retained for crash fallback
Embedded font: Lilex Regular and Bold (SIL Open Font License 1.1)
EOF

tar -czf "$ARCHIVE" -C "$PACKAGE_ROOT" .
printf '%s\n' "$ARCHIVE"
