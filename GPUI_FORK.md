# Central GPUI fork

This repository is the shared GPUI source for RustTrados, ImplVisualizer, and
HTML renderer experiments.

## Provenance

- Upstream repository: `https://github.com/zed-industries/zed.git`
- Upstream base: `ea3d0f7abeb3dc5d0954d6d3fff453af5b0c7af9`
- Maintained branch: `central/gpui`
- The transform/text-raster change was validated originally in
  `ImplVisualizer/gpui-compare/upstream`.

The workspace manifest is intentionally limited to GPUI and its dependency
closure. The remaining Zed sources are retained so upstream commits can still
be merged without reconstructing repository history.

## Patch stack

Each downstream behavior is kept in an independent commit:

1. Limit the workspace to GPUI and required support crates.
2. Port transform-aware painting and text rasterization.
3. Skip line-background glyph traversal when no background is present.
4. Expose transform-aware painted glyph right-overhang measurement.
5. Support named extension filters in native file dialogs.
6. Preserve standard versus numeric-keypad key location.

The old RustTrados patches that suppress text lifecycle panics, restrict image
formats, hide platform APIs, or alter vendored example targets are deliberately
not part of the shared fork. Those are either policy decisions or artifacts of
the older vendored crate.

## Validation

Run:

```sh
cargo check -p gpui -p gpui_platform -p gpui_wgpu -p gpui_linux -p gpui_web
cargo test -p gpui --lib
```

Platform-specific changes under `gpui_macos` and `gpui_windows` should also be
checked in native CI before publishing a revision for production use.

## Consuming the fork

Publish the repository to a stable remote, then pin every consumer to an exact
commit rather than a branch or Cargo's `0.2.2` version label. All split GPUI
packages must come from the same revision.

Keep the official Zed repository configured as the `upstream` remote. Reserve
`origin` for the hosted central fork.
