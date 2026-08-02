#!/bin/sh

WORKDIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
LOG="$WORKDIR/gpui-kobo.log"
IMAGE=/tmp/gpui-kobo-button.pgm
NICKEL_WAS_RUNNING=0

if pidof nickel >/dev/null 2>&1; then
    NICKEL_WAS_RUNNING=1
fi

restart_nickel() {
    if [ "$NICKEL_WAS_RUNNING" -ne 1 ] || pidof nickel >/dev/null 2>&1; then
        return
    fi
    (
        cd /
        export LD_LIBRARY_PATH=/usr/local/Kobo
        /usr/local/Kobo/hindenburg >/dev/null 2>&1 &
        LIBC_FATAL_STDERR_=1 /usr/local/Kobo/nickel \
            -platform kobo -skipFontLoad >/dev/null 2>&1 &
        if command -v udevadm >/dev/null 2>&1; then
            udevadm trigger >/dev/null 2>&1 &
        fi
    )
}

trap restart_nickel 0 1 2 15

{
    printf '\n[%s] starting GPUI Kobo test\n' "$(date)"
    cd "$WORKDIR" || exit 1
    chmod +x gpui-kobo-button fbink

    if [ "$NICKEL_WAS_RUNNING" -eq 1 ]; then
        sync
        killall -TERM nickel hindenburg sickel fickel adobehost foxitpdf iink \
            >/dev/null 2>&1 || true
        sleep 2
    fi

    if ! FBINK_BIN="$WORKDIR/fbink" "$WORKDIR/gpui-kobo-button" \
        --interactive \
        --timeout-seconds "${GPUI_KOBO_TIMEOUT_SECONDS:-45}" \
        --output "$IMAGE"; then
        "$WORKDIR/fbink" -q -c -f -p -m -M -h \
            "GPUI Kobo test failed; see gpui-kobo.log"
        sleep 10
        exit 1
    fi

    printf '[%s] GPUI Kobo test complete\n' "$(date)"
} >>"$LOG" 2>&1

exit 0
