# Central GPUI fork

This repository is the shared GPUI source for RustTrados, ImplVisualizer, and
HTML renderer experiments.

## Provenance

- Upstream repository: `https://github.com/zed-industries/zed.git`
- Upstream base: `f36aec822be697df9049fed020b593147c93b4cf`
- Maintained branch: `central/gpui`

The workspace manifest is intentionally limited to GPUI and its dependency
closure. The remaining Zed sources are retained so upstream commits can still
be merged without reconstructing repository history.

## Patch stack

The maintained downstream surface is:

1. Limit the workspace to GPUI and required support crates.
2. Provide the production Kobo platform and CPU/e-ink renderer.
3. Expose batch glyph painting and painted-glyph right-overhang measurement.
4. Preserve standard versus numeric-keypad key location on every backend.
5. Support named extension filters in native file dialogs.
6. Gate the broader image formats while retaining optional WebP decoding.
7. Provide configurable-scale headless contexts and a WGPU headless renderer.
8. Coalesce Wayland resize work with grow-only WGPU surface capacity.

The experimental transform-aware widget/text-raster patch is not retained.
ImplVisualizer's production graph applies its own pan and zoom, and GPUI widgets
continue to use ordinary window coordinates. `container_query` comes directly
from upstream GPUI.

## Validation

The host-side compile checks are:

```sh
cargo check --locked -p gpui --lib
cargo check --locked -p gpui_wgpu --features test-support
cargo check --locked -p gpui_platform --features test-support,wayland,x11
cargo check --locked -p gpui_kobo
```

Platform-specific changes under `gpui_macos` and `gpui_windows` should also be
checked in native CI before publishing a revision for production use.

## Consuming the fork

Publish the repository to a stable remote, then pin every consumer to an exact
commit rather than a branch or Cargo's `0.2.2` version label. All split GPUI
packages must come from the same revision.

Keep the official Zed repository configured as the `upstream` remote. Reserve
`origin` for the hosted central fork.
