use anyhow::Result;
use gpui::{
    BackgroundExecutor, Bounds, CapturePixelFormat, DevicePixels, RegionCapture, Size, Task,
    capture_image,
};
use image::RgbaImage;
use metal::{
    Buffer, CommandBuffer, CommandBufferRef, DeviceRef, MTLBlitOption, MTLCommandBufferStatus,
    MTLOrigin, MTLResourceOptions, MTLSize, TextureRef,
};
use std::slice;

#[derive(Default)]
pub(crate) struct MetalCaptures(Vec<RegionCapture>);

pub(crate) struct MetalReadback {
    request: RegionCapture,
    buffer: Buffer,
    commands: CommandBuffer,
    extent: Size<DevicePixels>,
    stride: usize,
    premultiplied: bool,
}

impl MetalCaptures {
    pub(crate) fn request(
        &mut self,
        bounds: Bounds<DevicePixels>,
        executor: &BackgroundExecutor,
    ) -> Task<Result<RgbaImage>> {
        let (request, task) = RegionCapture::new(bounds, executor);
        self.0.push(request);
        task
    }

    pub(crate) fn fail(&mut self, message: &str) {
        for request in self.0.drain(..) {
            request.complete(Err(anyhow::anyhow!("{message}")));
        }
    }

    pub(crate) fn encode(
        &mut self,
        device: &DeviceRef,
        source: &TextureRef,
        commands: &CommandBufferRef,
        viewport: Size<DevicePixels>,
        unified_memory: bool,
        opaque: bool,
    ) -> Vec<MetalReadback> {
        let mut readbacks = Vec::new();
        for request in self.0.drain(..) {
            if request.is_cancelled() {
                continue;
            }
            let bounds = match request.bounds(viewport) {
                Ok(bounds) => bounds,
                Err(error) => {
                    request.complete(Err(error));
                    continue;
                }
            };
            let width = bounds.size.width.0 as usize;
            let height = bounds.size.height.0 as usize;
            let stride = (width * 4).div_ceil(256) * 256;
            let options = if unified_memory {
                MTLResourceOptions::StorageModeShared
            } else {
                MTLResourceOptions::StorageModeManaged
            };
            let buffer = device.new_buffer((stride * height) as u64, options);
            let blit = commands.new_blit_command_encoder();
            blit.copy_from_texture_to_buffer(
                source,
                0,
                0,
                MTLOrigin {
                    x: bounds.origin.x.0 as u64,
                    y: bounds.origin.y.0 as u64,
                    z: 0,
                },
                MTLSize {
                    width: width as u64,
                    height: height as u64,
                    depth: 1,
                },
                &buffer,
                0,
                stride as u64,
                (stride * height) as u64,
                MTLBlitOption::empty(),
            );
            if !unified_memory {
                blit.synchronize_resource(&buffer);
            }
            blit.end_encoding();
            readbacks.push(MetalReadback {
                request,
                buffer,
                commands: commands.to_owned(),
                extent: bounds.size,
                stride,
                premultiplied: !opaque,
            });
        }
        readbacks
    }
}

impl MetalReadback {
    pub(crate) fn complete(self) {
        let Self {
            request,
            buffer,
            commands,
            extent,
            stride,
            premultiplied,
        } = self;
        // Called after commit, so waiting cannot race an uncommitted command
        // buffer. The retained buffer stays alive until its bytes are copied.
        request.complete_in_background(move || {
            commands.wait_until_completed();
            anyhow::ensure!(
                commands.status() == MTLCommandBufferStatus::Completed,
                "Metal capture did not complete: {:?}",
                commands.status()
            );
            let pointer = buffer.contents().cast::<u8>();
            anyhow::ensure!(!pointer.is_null(), "Metal returned no captured pixels");
            let bytes =
                unsafe { slice::from_raw_parts(pointer, stride * extent.height.0 as usize) };
            capture_image(
                bytes,
                extent,
                stride,
                CapturePixelFormat::Bgra,
                premultiplied,
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, point, size};
    use metal::{Device, MTLPixelFormat, MTLRegion, MTLStorageMode, TextureDescriptor};

    #[gpui::test]
    async fn copies_a_metal_region_after_command_completion(cx: &mut TestAppContext) {
        let device = Device::system_default().expect("Metal device");
        let unified = device.has_unified_memory();
        let descriptor = TextureDescriptor::new();
        descriptor.set_width(5);
        descriptor.set_height(4);
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        descriptor.set_storage_mode(if unified {
            MTLStorageMode::Shared
        } else {
            MTLStorageMode::Managed
        });
        let source = device.new_texture(&descriptor);
        let pixels: Vec<u8> = (0..4u8)
            .flat_map(|y| (0..5u8).flat_map(move |x| [x, y, 20, 255]))
            .collect();
        source.replace_region(
            MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize {
                    width: 5,
                    height: 4,
                    depth: 1,
                },
            },
            0,
            pixels.as_ptr().cast(),
            20,
        );
        let queue = device.new_command_queue();
        let commands = queue.new_command_buffer();
        let mut captures = MetalCaptures::default();
        let task = captures.request(
            Bounds::new(
                point(DevicePixels(1), DevicePixels(1)),
                size(DevicePixels(3), DevicePixels(2)),
            ),
            &cx.background_executor,
        );
        let readbacks = captures.encode(
            &device,
            &source,
            commands,
            size(DevicePixels(5), DevicePixels(4)),
            unified,
            true,
        );
        commands.commit();
        for readback in readbacks {
            readback.complete();
        }
        let image = task.await.expect("read Metal capture");
        assert_eq!(image.dimensions(), (3, 2));
        for (x, y, pixel) in image.enumerate_pixels() {
            assert_eq!(pixel.0, [20, (y + 1) as u8, (x + 1) as u8, 255]);
        }
    }
}
