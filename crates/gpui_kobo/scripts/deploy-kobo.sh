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
ARCHIVE=$("$SCRIPT_DIR/package-kobo.sh")
tar -xzf "$ARCHIVE" -C "$DEVICE_ROOT"
sync

cat <<EOF
Installed the GPUI test under:
  $DEVICE_ROOT/.adds/gpui-kobo

Safely eject the Kobo. If NickelMenu is installed, select:
  GPUI Kobo test

The test stops Nickel and displays a GPUI-rendered text library. Drag the list
to scroll and tap EXIT to return to Nickel. A 45-second timeout recovers
automatically. The log is in .adds/gpui-kobo/gpui-kobo.log.
EOF
