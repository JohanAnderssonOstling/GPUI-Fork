# gpui_kobo

Experimental CPU renderer for determining whether GPUI can support a Kobo
framebuffer backend. This is not a Kobo application or a complete GPUI platform.

The first spike uses GPUI's headless test platform to build a real scene and
supports only the primitives required to render one button:

- solid quads;
- monochrome glyph sprites;
- subpixel glyph sprites converted to grayscale.

Framebuffer access, device input, e-ink update modes, images, paths, shadows,
and production platform integration are intentionally out of scope.
