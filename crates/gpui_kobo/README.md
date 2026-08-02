# gpui_kobo

Deployable feasibility test for determining whether GPUI can support a Kobo
framebuffer backend. This is intentionally one rendered button, not a Kobo
application or a complete GPUI platform.

The test uses GPUI's headless platform to lay out a real GPUI view and a small
CPU renderer to rasterize its scene. The ARM executable writes a grayscale PGM
and invokes a separately bundled FBInk CLI to present it on e-ink hardware.

Supported GPUI primitives are deliberately limited to what the test needs:

- solid quads;
- monochrome glyph sprites;
- subpixel glyph sprites converted to grayscale.

Paths, shadows, images, surfaces, continuous event handling, and production
platform integration remain out of scope. The test icon is made from GPUI quads
so the device build does not require `gpui_wgpu`, a GPU, or system fonts.

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
source archive and GPLv3 license alongside its static CLI binary.

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
3. renders and displays the GPUI button through FBInk using a full GC16 refresh;
4. leaves the result visible for 15 seconds;
5. restarts Nickel even when the test command fails or is terminated.

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
