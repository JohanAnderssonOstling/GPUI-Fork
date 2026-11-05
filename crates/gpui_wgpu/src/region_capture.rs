use anyhow::{Context as _, Result};
use gpui::{
    BackgroundExecutor, Bounds, CapturePixelFormat, DevicePixels, RegionCapture, Size, Task,
    capture_image, size,
};
use image::RgbaImage;
use std::{sync::Arc, time::Duration};

#[derive(Default)]
pub(crate) struct RegionCaptures(Vec<RegionCapture>);

struct Readback {
    device: Arc<wgpu::Device>,
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    stride: u32,
    bgra: bool,
    premultiplied: bool,
}

impl RegionCaptures {
    pub(crate) fn request(
        &mut self,
        bounds: Bounds<DevicePixels>,
        executor: &BackgroundExecutor,
    ) -> Task<Result<RgbaImage>> {
        let (request, task) = RegionCapture::new(bounds, executor);
        self.0.push(request);
        task
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn fail(&mut self, message: &str) {
        for capture in self.0.drain(..) {
            capture.complete(Err(anyhow::anyhow!("{message}")));
        }
    }

    pub(crate) fn submit(
        &mut self,
        texture: &wgpu::Texture,
        device: &Arc<wgpu::Device>,
        queue: &wgpu::Queue,
        viewport: Size<DevicePixels>,
        premultiplied: bool,
    ) {
        for capture in self.0.drain(..) {
            if capture.is_cancelled() {
                continue;
            }
            let result = (|| {
                let bounds = capture.bounds(viewport)?;
                let (x, y, width, height) = (
                    bounds.origin.x.0 as u32,
                    bounds.origin.y.0 as u32,
                    bounds.size.width.0 as u32,
                    bounds.size.height.0 as u32,
                );
                let format = texture.format();
                anyhow::ensure!(
                    matches!(
                        format,
                        wgpu::TextureFormat::Bgra8Unorm
                            | wgpu::TextureFormat::Bgra8UnormSrgb
                            | wgpu::TextureFormat::Rgba8Unorm
                            | wgpu::TextureFormat::Rgba8UnormSrgb
                    ),
                    "Unsupported capture texture format: {format:?}"
                );
                let stride = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                    * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("gpui_region_readback"),
                    size: u64::from(stride) * u64::from(height),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("gpui_region_capture"),
                });
                encoder.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x, y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buffer,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(stride),
                            rows_per_image: Some(height),
                        },
                    },
                    wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                );
                queue.submit([encoder.finish()]);
                Ok(Readback {
                    device: Arc::clone(device),
                    buffer,
                    width,
                    height,
                    stride,
                    bgra: matches!(
                        format,
                        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
                    ),
                    premultiplied,
                })
            })();
            match result {
                Ok(readback) => capture.complete_in_background(move || readback.read()),
                Err(error) => capture.complete(Err(error)),
            }
        }
    }
}

impl Readback {
    fn read(self) -> Result<RgbaImage> {
        let slice = self.buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            if sender.send(result).is_err() {
                log::debug!("Region readback was cancelled");
            }
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(10)),
            })
            .context("Failed to wait for region capture")?;
        receiver
            .recv_timeout(Duration::from_secs(10))
            .context("Region readback did not complete")?
            .context("Failed to map captured pixels")?;
        let mapped = slice.get_mapped_range();
        let result = capture_image(
            &mapped,
            size(
                DevicePixels(self.width as i32),
                DevicePixels(self.height as i32),
            ),
            self.stride as usize,
            if self.bgra {
                CapturePixelFormat::Bgra
            } else {
                CapturePixelFormat::Rgba
            },
            self.premultiplied,
        );
        drop(mapped);
        self.buffer.unmap();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    #[gpui::test]
    #[cfg(feature = "test-support")]
    async fn copies_only_the_requested_gpu_pixels_with_padded_rows(cx: &mut gpui::TestAppContext) {
        let context = crate::WgpuContext::new_headless().expect("headless GPU context");
        let texture = context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("region_capture_test"),
            size: wgpu::Extent3d {
                width: 5,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let pixels: Vec<u8> = (0..4u8)
            .flat_map(|y| (0..5u8).flat_map(move |x| [x, y, 20, 255]))
            .collect();
        context.queue.write_texture(
            texture.as_image_copy(),
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(20),
                rows_per_image: Some(4),
            },
            wgpu::Extent3d {
                width: 5,
                height: 4,
                depth_or_array_layers: 1,
            },
        );
        let mut captures = RegionCaptures::default();
        let task = captures.request(
            Bounds::new(
                point(DevicePixels(1), DevicePixels(1)),
                size(DevicePixels(3), DevicePixels(2)),
            ),
            &cx.background_executor,
        );
        captures.submit(
            &texture,
            &context.device,
            &context.queue,
            size(DevicePixels(5), DevicePixels(4)),
            false,
        );
        let image = task.await.expect("GPU region capture");
        assert_eq!(image.dimensions(), (3, 2));
        for (x, y, pixel) in image.enumerate_pixels() {
            assert_eq!(pixel.0, [20, (y + 1) as u8, (x + 1) as u8, 255]);
        }
    }
}
