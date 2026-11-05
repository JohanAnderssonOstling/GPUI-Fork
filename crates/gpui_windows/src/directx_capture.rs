use anyhow::{Context as _, Result};
use gpui::{
    BackgroundExecutor, Bounds, CapturePixelFormat, DevicePixels, RegionCapture, Task,
    WindowBackgroundAppearance, capture_image, size,
};
use image::RgbaImage;
use std::{
    slice,
    time::{Duration, Instant},
};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::DXGI_ERROR_WAS_STILL_DRAWING};

#[derive(Default)]
pub(crate) struct DirectXCaptures {
    requests: Vec<RegionCapture>,
    readbacks: Vec<Readback>,
}

struct Readback {
    request: RegionCapture,
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    premultiplied: bool,
    started: Instant,
}

impl DirectXCaptures {
    pub(crate) fn request(
        &mut self,
        bounds: Bounds<DevicePixels>,
        executor: &BackgroundExecutor,
    ) -> Task<Result<RgbaImage>> {
        let (request, task) = RegionCapture::new(bounds, executor);
        self.requests.push(request);
        task
    }

    pub(crate) fn is_pending(&self) -> bool {
        !self.requests.is_empty() || !self.readbacks.is_empty()
    }

    pub(crate) fn fail(&mut self, message: &str) {
        for request in self
            .requests
            .drain(..)
            .chain(self.readbacks.drain(..).map(|readback| readback.request))
        {
            request.complete(Err(anyhow::anyhow!("{message}")));
        }
    }

    pub(crate) fn submit(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        appearance: WindowBackgroundAppearance,
    ) {
        if self.requests.is_empty() {
            return;
        }
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        for request in self.requests.drain(..) {
            if request.is_cancelled() {
                continue;
            }
            let result = (|| {
                let bounds = request.bounds(size(
                    DevicePixels(source_desc.Width as i32),
                    DevicePixels(source_desc.Height as i32),
                ))?;
                let width = bounds.size.width.0 as u32;
                let height = bounds.size.height.0 as u32;
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..source_desc
                };
                let mut texture = None;
                unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
                let texture = texture.context("Direct3D did not create a capture texture")?;
                let region = D3D11_BOX {
                    left: bounds.left().0 as u32,
                    top: bounds.top().0 as u32,
                    front: 0,
                    right: bounds.right().0 as u32,
                    bottom: bounds.bottom().0 as u32,
                    back: 1,
                };
                unsafe {
                    context.CopySubresourceRegion(&texture, 0, 0, 0, 0, source, 0, Some(&region))
                };
                Ok((texture, width, height))
            })();
            match result {
                Ok((texture, width, height)) => self.readbacks.push(Readback {
                    request,
                    texture,
                    width,
                    height,
                    premultiplied: appearance != WindowBackgroundAppearance::Opaque,
                    started: Instant::now(),
                }),
                Err(error) => request.complete(Err(error)),
            }
        }
    }

    pub(crate) fn poll(&mut self, context: &ID3D11DeviceContext) {
        let mut waiting = Vec::new();
        for readback in self.readbacks.drain(..) {
            if readback.request.is_cancelled() {
                continue;
            }
            if readback.started.elapsed() > Duration::from_secs(10) {
                readback
                    .request
                    .complete(Err(anyhow::anyhow!("Direct3D capture timed out")));
                continue;
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            match unsafe {
                context.Map(
                    &readback.texture,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    waiting.push(readback)
                }
                Err(error) => readback.request.complete(Err(error.into())),
                Ok(()) => {
                    let result = (|| {
                        anyhow::ensure!(
                            !mapped.pData.is_null(),
                            "Direct3D returned no captured pixels"
                        );
                        let stride = mapped.RowPitch as usize;
                        let length = (readback.height as usize - 1)
                            .checked_mul(stride)
                            .and_then(|offset| offset.checked_add(readback.width as usize * 4))
                            .context("Direct3D capture size overflow")?;
                        let bytes =
                            unsafe { slice::from_raw_parts(mapped.pData.cast::<u8>(), length) };
                        capture_image(
                            bytes,
                            size(
                                DevicePixels(readback.width as i32),
                                DevicePixels(readback.height as i32),
                            ),
                            stride,
                            CapturePixelFormat::Bgra,
                            readback.premultiplied,
                        )
                    })();
                    unsafe { context.Unmap(&readback.texture, 0) };
                    readback.request.complete(result);
                }
            }
        }
        self.readbacks = waiting;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, point};
    use windows::Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_WARP,
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
        },
    };

    #[gpui::test]
    async fn copies_a_clipped_direct3d_region(cx: &mut TestAppContext) {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .expect("create software D3D11 device");
        let device = device.expect("D3D11 device");
        let context = context.expect("D3D11 context");
        let pixels: Vec<u8> = (0..4u8)
            .flat_map(|y| (0..5u8).flat_map(move |x| [x, y, 20, 255]))
            .collect();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 5,
            Height: 4,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: 20,
            SysMemSlicePitch: 0,
        };
        let mut source = None;
        unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut source)) }
            .expect("create capture source");
        let mut captures = DirectXCaptures::default();
        let task = captures.request(
            Bounds::new(
                point(DevicePixels(1), DevicePixels(1)),
                size(DevicePixels(10), DevicePixels(2)),
            ),
            &cx.background_executor,
        );
        captures.submit(
            &device,
            &context,
            &source.expect("source texture"),
            WindowBackgroundAppearance::Opaque,
        );
        unsafe { context.Flush() };
        let deadline = Instant::now() + Duration::from_secs(5);
        while captures.is_pending() {
            captures.poll(&context);
            assert!(Instant::now() < deadline, "D3D11 capture did not complete");
            cx.background_executor.timer(Duration::from_millis(1)).await;
        }
        let image = task.await.expect("read captured image");
        assert_eq!(image.dimensions(), (4, 2));
        for (x, y, pixel) in image.enumerate_pixels() {
            assert_eq!(pixel.0, [20, (y + 1) as u8, (x + 1) as u8, 255]);
        }
    }
}
