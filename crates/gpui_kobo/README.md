# gpui_kobo

Deployable feasibility backend for determining whether GPUI can support Kobo
e-readers. This remains a focused backend test, not a Kobo reader application.

The test retains a real GPUI view, shapes an embedded Lilex font with GPUI's
Cosmic text system, and rasterizes the scene with a small CPU renderer. Raw
evdev touch drags become GPUI mouse and scroll events. Consecutive GPUI frames
are diffed to derive the exact damage rectangle sent to the separately bundled
FBInk CLI.

Supported GPUI primitives are deliberately limited to what the test needs:

- solid quads;
- monochrome glyph sprites;
- subpixel glyph sprites converted to grayscale.

Paths, shadows, images, surfaces, kinetic gestures, and production platform
integration remain out of scope. `gpui_wgpu::CosmicTextSystem` is reused for
text shaping and glyph rasterization only; presentation remains CPU-only and
does not require a GPU or system fonts.

## Host smoke run

```sh
cargo run -p gpui_kobo --bin gpui-kobo-button -- \
    --no-display --output /tmp/gpui-kobo-button.pgm
```

## Build a transferable Kobo package

Prerequisites are `curl`, `make`, `rustup`, `sha256sum`, and `tar`. The build
downloads and verifies a pinned ARMv7 hard-float musl toolchain (about 98 MiB),
installs Rust's `armv7-unknown-linux-musleabihf` standard library, builds the
GPUI executable, and builds FBInk `v1.25.0` with image support.

```sh
./crates/gpui_kobo/scripts/package-kobo.sh
```

The result is:

```text
target/gpui-kobo/gpui-kobo-test-armv7.tar.gz
```

Both external downloads are SHA-256 pinned. The package includes FBInk's exact
source archive and GPLv3 license alongside its static CLI binary. Lilex Regular
and Bold are embedded in the executable and its SIL Open Font License is also
included.

## Transfer over USB

This route expects NickelMenu to already be installed on the Kobo. Connect the
Kobo, select **Connect** on its USB prompt, find its mounted `KOBOeReader` root,
then run:

```sh
./crates/gpui_kobo/scripts/deploy-kobo.sh /run/media/$USER/KOBOeReader
```

Use the actual mount path on the host. Safely eject the device after the script
finishes. On the Kobo, open NickelMenu and select **GPUI Kobo test**.

The launcher:

1. records whether Nickel was running;
2. stops Nickel and other framebuffer-owning reader processes;
3. renders and displays the GPUI text library through FBInk using full GC16;
4. routes touch drags through GPUI scrolling and presents pixel-derived damage
   rectangles with partial A2 refreshes;
5. exits through the GPUI EXIT control or after 45 seconds;
6. restarts Nickel even when the test command fails or is terminated.

Diagnostics are appended to:

```text
/mnt/onboard/.adds/gpui-kobo/gpui-kobo.log
```

For an SSH-enabled Kobo, the same installed test can be started directly:

```sh
ssh root@KOBO_IP /mnt/onboard/.adds/gpui-kobo/run.sh
```

## Device scope and recovery

The binary targets ARMv7 Linux with the hard-float ABI and is statically linked
against musl. This covers the ARMv7 Kobo generation used by Plato and similar
third-party readers; it is not an assertion of support for every future Kobo
SoC. The framebuffer model matrix is delegated to FBInk.

If Nickel does not return after the test, reboot the Kobo with its power button.
Removing these two paths over USB uninstalls the test:

```text
.adds/gpui-kobo
.adds/nm/gpui-kobo
```

## Licensing boundary

`gpui-kobo-button` and this crate are Apache-2.0. The process invokes, but does
not link to, the separately distributed GPLv3+ FBInk CLI. The device package
retains the separate binaries and includes FBInk's corresponding pinned source
and license. FBInk is built with:

```sh
make static MINIMAL=1 BITMAP=1 IMAGE=1 LDFLAGS=-static
```
## Interactive backend test

The packaged launcher discovers the Kobo touchscreen through
`/dev/input/event*`, maps its absolute coordinates to the 600 by 800 GPUI test
canvas, and displays a text library with more rows than fit on screen. Drag the
list upward and downward to exercise GPUI-native scrolling. Every rendered
frame is compared with the previous one, and FBInk receives only the resulting
changed-pixel bounds. Tap EXIT to restart Nickel. If input discovery or touch
handling fails, the launcher recovers after 45 seconds; override that fallback
with `GPUI_KOBO_TIMEOUT_SECONDS`.

## Automated device preflight

Before stopping Nickel, the launcher runs a headless on-device self-test. It
uses synthetic touch input and fails the launch if embedded text does not
rasterize, touch-down or touch-move causes a render, finger-up causes anything
other than one render, scroll damage escapes the list viewport, or EXIT hit
testing requires a framebuffer update. The log records structured `SELFTEST
PASS` lines with render counts, damage bounds, and timings. This keeps routine
device testing log-driven; manual checks are limited to visual quality and one
physical swipe.
