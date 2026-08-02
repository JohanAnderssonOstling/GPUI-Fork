#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    printf 'Usage: %s /path/to/KOBOeReader\n' "$0" >&2
    exit 2
fi

DEVICE_ROOT=${1%/}
if [ ! -d "$DEVICE_ROOT/.kobo" ]; then
    printf '%s does not look like a mounted Kobo filesystem (.kobo is missing)\n' \
        "$DEVICE_ROOT" >&2
    exit 1
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CRATE_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(CDPATH= cd -- "$CRATE_DIR/../.." && pwd)
BUILD_ROOT=${KOBO_BUILD_ROOT:-"$REPO_ROOT/target/gpui-kobo"}
PACKAGE_ROOT="$BUILD_ROOT/package"

"$SCRIPT_DIR/package-kobo.sh" >/dev/null

mkdir -p "$DEVICE_ROOT/.adds/gpui-kobo" "$DEVICE_ROOT/.adds/nm"
cp -R "$PACKAGE_ROOT/.adds/gpui-kobo/." "$DEVICE_ROOT/.adds/gpui-kobo/"
cp "$PACKAGE_ROOT/.adds/nm/gpui-kobo" "$DEVICE_ROOT/.adds/nm/gpui-kobo"
sync

cat <<EOF
Installed the GPUI test under:
  $DEVICE_ROOT/.adds/gpui-kobo

Safely eject the Kobo. If NickelMenu is installed, select:
  GPUI Kobo test

The test stops Nickel, displays the GPUI-rendered button for 15 seconds, and
then restarts Nickel. Its log is written to .adds/gpui-kobo/gpui-kobo.log.
EOF
