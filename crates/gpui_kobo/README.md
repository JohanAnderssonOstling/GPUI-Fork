# gpui_kobo

Production-oriented, single-window Kobo backend for GPUI. It implements GPUI's
`Platform`, `PlatformDisplay`, `PlatformWindow`, and `PlatformDispatcher`
contracts for a fullscreen e-ink application. The included text library remains
a hardware smoke application rather than a reader, but the same backend can
host the project's EPUB root view through normal `Application::with_platform`
startup.

The application runs in GPUI production mode, shapes an embedded Lilex font
with GPUI's Cosmic text system, and rasterizes the scene with a small CPU renderer. Raw
evdev input becomes GPUI mouse and scroll events. Consecutive GPUI frames are
diffed to derive exact damage. ARM builds link FBInk directly and pass grayscale
buffers through `fbink_print_raw_data`; normal presentation uses neither image
files nor child processes. The FBInk CLI remains only as a crash-screen fallback.

Supported GPUI primitives are deliberately limited to what the test needs:

- solid rounded quads with borders;
- linear gradients flattened to a representative grayscale fill;
- monochrome glyph sprites;
- subpixel glyph sprites converted to grayscale;
- straight underlines;
- wavy underlines;
- RGBA polychrome sprites converted to grayscale with rounded clipping;
- affine-transformed glyph sprites;
- triangulated straight and quadratic paths;
- shadow primitives accepted but intentionally omitted.

Native paint surfaces and GPU-only effects remain out of scope.
`gpui_wgpu::CosmicTextSystem` is reused for
text shaping and glyph rasterization only; presentation remains CPU-only and
does not require a GPU or system fonts.

## Application integration

Create the platform before the GPUI application and pass it to the normal
production constructor:

```rust
let platform = KoboPlatform::new(KoboPlatformOptions::default())?;
Application::new_inaccessible(platform.clone())
    .with_assets(epub_assets)
    .run(|cx| {
        cx.open_window(window_options, |window, cx| {
            cx.new(|cx| EpubReader::new(window, cx))
        })
        .expect("failed to open the Kobo EPUB window");
    });
if let Some(error) = platform.take_error() {
    return Err(error);
}
```

The implemented production services are the main/background executors, one
display, one fullscreen window, request-frame callbacks, CPU scene drawing,
damage and refresh scheduling, FBInk presentation, touch gestures, page/home/
power input, rotation reinitialization, and quit/timeout recovery. Desktop-only
services such as multiple windows, decorations, menus, dialogs, URL launching,
screen capture, cursor display, and persistent credential storage are explicit
no-ops or unsupported responses.

The crate does not enable GPUI's `test-support` feature and does not use
`HeadlessAppContext` in the deployed path.

Kobo-specific behavior is explicit backend policy:

- `ScreenGeometry` parses FBInk viewport, panel size, origin, rotation, and device identity;
- touch coordinates support inferred axis swapping and a `GPUI_KOBO_TOUCH_TRANSFORM=swap,invert-x,invert-y` override;
- calibration ranges can be overridden with `GPUI_KOBO_TOUCH_CALIBRATION=x_min,x_max,y_min,y_max`;
- gestures include tap, swipe, long press, and cancellation and are classified only at finger release, preserving the no-render-during-drag guard;
- a Kobo runtime polls touch and hardware buttons, handles page-button scrolling, and reinitializes FBInk after rotation or wake;
- scroll releases use one A2 damage update, other changes use partial GC16, and cleanup uses full GC16 after six fast updates or three accumulated screen areas;
- queued invalidations are coalesced and an idle GC16 cleanup runs after a fast-update sequence;
- `--self-test --no-display` checks geometry, gesture and refresh policy, text rasterization, repaint suppression, damage bounds, and EXIT behavior before Nickel is stopped.

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
source archive and GPLv3 license. The ARM executable statically links FBInk, so
the distributed combined executable is governed by GPLv3; the crate's original
Apache-2.0 source remains GPLv3-compatible. Lilex Regular
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
3. opens one linked FBInk session and displays the GPUI text library using full GC16;
4. routes touch, page buttons, wake, and rotation through the Kobo runtime and
   presents in-memory damage on release, with count-, area-, and idle-triggered
   full GC16 cleanup;
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

The Rust sources remain Apache-2.0. The packaged ARM executable statically links
GPLv3+ FBInk, making that combined distributed binary GPLv3. The device package
includes FBInk's corresponding pinned source and license. FBInk is built with:

```sh
make static MINIMAL=1 BITMAP=1 IMAGE=1 LDFLAGS=-static
```
## Interactive backend test

The packaged launcher discovers the Kobo touchscreen through
`/dev/input/event*`, maps its absolute coordinates to the 600 by 800 GPUI test
canvas, polls page/home/power buttons, and displays a text library with more rows
than fit on screen. Drag the list or use page buttons to exercise GPUI-native
scrolling. Every rendered frame is compared with the previous one, and linked
FBInk receives only the resulting changed-pixel bounds. Tap EXIT to restart
Nickel. If input discovery or touch
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
