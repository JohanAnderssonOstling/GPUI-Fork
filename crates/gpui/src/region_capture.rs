use crate::{BackgroundExecutor, Bounds, DevicePixels, Size, Task, point, size};
use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use image::RgbaImage;

/// A pending frame-region request shared by native renderers.
pub struct RegionCapture {
    bounds: Bounds<DevicePixels>,
    sender: oneshot::Sender<Result<RgbaImage>>,
    executor: BackgroundExecutor,
}

impl RegionCapture {
    /// Creates a request and the task that resolves when the renderer completes it.
    pub fn new(
        bounds: Bounds<DevicePixels>,
        executor: &BackgroundExecutor,
    ) -> (Self, Task<Result<RgbaImage>>) {
        let (sender, receiver) = oneshot::channel();
        let task = executor.spawn(async move {
            receiver
                .await
                .context("Window closed before capture completed")?
        });
        (
            Self {
                bounds,
                sender,
                executor: executor.clone(),
            },
            task,
        )
    }

    /// Clips this request to the frame's visible extent.
    pub fn bounds(&self, viewport: Size<DevicePixels>) -> Result<Bounds<DevicePixels>> {
        clip_capture_region(self.bounds, viewport)
    }

    /// Whether the caller has dropped the capture task.
    pub fn is_cancelled(&self) -> bool {
        self.sender.is_canceled()
    }

    /// Delivers pixels or a renderer error to the caller.
    pub fn complete(self, result: Result<RgbaImage>) {
        if self.sender.send(result).is_err() {
            log::debug!("Region capture was cancelled");
        }
    }

    /// Reads completed GPU work on the background executor.
    /// Renderers must only use this when their readback resources are thread-safe.
    pub fn complete_in_background(self, read: impl FnOnce() -> Result<RgbaImage> + Send + 'static) {
        self.executor
            .clone()
            .spawn(async move {
                self.complete(read());
            })
            .detach();
    }
}

/// Channel order of a captured 8-bit texture.
#[derive(Clone, Copy)]
pub enum CapturePixelFormat {
    /// Red, green, blue, alpha.
    Rgba,
    /// Blue, green, red, alpha.
    Bgra,
}

/// Packs padded GPU rows into a straight-alpha RGBA image.
pub fn capture_image(
    bytes: &[u8],
    extent: Size<DevicePixels>,
    stride: usize,
    format: CapturePixelFormat,
    premultiplied: bool,
) -> Result<RgbaImage> {
    anyhow::ensure!(
        extent.width.0 > 0 && extent.height.0 > 0,
        "Captured image is empty"
    );
    let width = extent.width.0 as usize;
    let height = extent.height.0 as usize;
    let row_bytes = width.checked_mul(4).context("Capture row size overflow")?;
    anyhow::ensure!(stride >= row_bytes, "Captured row stride is too small");
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(row_bytes))
        .context("Capture size overflow")?;
    anyhow::ensure!(
        bytes.len() >= required,
        "Captured pixel buffer is truncated"
    );
    let mut pixels = Vec::with_capacity(
        row_bytes
            .checked_mul(height)
            .context("Capture size overflow")?,
    );
    for row in bytes[..required].chunks(stride) {
        pixels.extend_from_slice(&row[..row_bytes]);
    }
    for pixel in pixels.chunks_exact_mut(4) {
        if matches!(format, CapturePixelFormat::Bgra) {
            pixel.swap(0, 2);
        }
        if premultiplied {
            let alpha = u32::from(pixel[3]);
            for channel in &mut pixel[..3] {
                *channel = if alpha == 0 {
                    0
                } else {
                    ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8
                };
            }
        }
    }
    RgbaImage::from_raw(width as u32, height as u32, pixels).context("Invalid captured image size")
}

fn clip_capture_region(
    bounds: Bounds<DevicePixels>,
    viewport: Size<DevicePixels>,
) -> Result<Bounds<DevicePixels>> {
    anyhow::ensure!(
        bounds.size.width.0 > 0 && bounds.size.height.0 > 0,
        "Capture rectangle is empty"
    );
    let left = bounds.left().0.max(0);
    let top = bounds.top().0.max(0);
    let right = i64::from(bounds.origin.x.0)
        .saturating_add(i64::from(bounds.size.width.0))
        .min(i64::from(viewport.width.0));
    let bottom = i64::from(bounds.origin.y.0)
        .saturating_add(i64::from(bounds.size.height.0))
        .min(i64::from(viewport.height.0));
    anyhow::ensure!(
        right > i64::from(left) && bottom > i64::from(top),
        "Capture rectangle is outside the window"
    );
    Ok(Bounds::new(
        point(DevicePixels(left), DevicePixels(top)),
        size(
            DevicePixels((right - i64::from(left)) as i32),
            DevicePixels((bottom - i64::from(top)) as i32),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn completion_and_window_close_resolve_the_capture_task(cx: &mut crate::TestAppContext) {
        let bounds = Bounds::new(
            point(DevicePixels(0), DevicePixels(0)),
            size(DevicePixels(1), DevicePixels(1)),
        );
        let (request, task) = RegionCapture::new(bounds, &cx.background_executor);
        request.complete(Ok(RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]))));
        assert_eq!(
            task.await.expect("completed capture").get_pixel(0, 0).0,
            [1, 2, 3, 255]
        );
        let (request, task) = RegionCapture::new(bounds, &cx.background_executor);
        drop(request);
        assert!(task.await.is_err());
    }

    #[test]
    fn clips_regions_and_rejects_invalid_extents() {
        let viewport = size(DevicePixels(100), DevicePixels(80));
        let bounds = |x, y, width, height| {
            Bounds::new(
                point(DevicePixels(x), DevicePixels(y)),
                size(DevicePixels(width), DevicePixels(height)),
            )
        };
        assert_eq!(
            clip_capture_region(bounds(-5, 10, 30, 90), viewport).unwrap(),
            bounds(0, 10, 25, 70)
        );
        assert!(clip_capture_region(bounds(100, 0, 10, 10), viewport).is_err());
        assert!(clip_capture_region(bounds(5, 5, 0, 10), viewport).is_err());
        assert_eq!(
            clip_capture_region(bounds(1, 1, i32::MAX, i32::MAX), viewport).unwrap(),
            bounds(1, 1, 99, 79)
        );
    }

    #[test]
    fn normalizes_channel_order_padding_and_alpha() {
        let extent = size(DevicePixels(1), DevicePixels(2));
        let bytes = [10, 20, 30, 128, 99, 99, 99, 99, 90, 60, 30, 255];
        let image = capture_image(&bytes, extent, 8, CapturePixelFormat::Bgra, true).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [60, 40, 20, 128]);
        assert_eq!(image.get_pixel(0, 1).0, [30, 60, 90, 255]);
        assert!(capture_image(&bytes[..8], extent, 8, CapturePixelFormat::Bgra, false).is_err());
        assert!(capture_image(&bytes, extent, 3, CapturePixelFormat::Rgba, false).is_err());
    }
}
