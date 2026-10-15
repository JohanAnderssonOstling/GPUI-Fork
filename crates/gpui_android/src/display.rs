use anyhow::Result;
use gpui::{Bounds, DevicePixels, DisplayId, Pixels, PlatformDisplay, Point, Size, px};
use std::cell::Cell;

#[derive(Debug)]
pub struct AndroidDisplay {
    id: DisplayId,
    uuid: uuid::Uuid,
    size: Cell<Size<Pixels>>,
}

// Safety: the display is only accessed from the android_main thread; GPUI's
// PlatformDisplay trait requires Send + Sync but never shares it across threads.
unsafe impl Send for AndroidDisplay {}
unsafe impl Sync for AndroidDisplay {}

impl AndroidDisplay {
    pub fn new() -> Self {
        AndroidDisplay {
            id: DisplayId::new(1),
            uuid: uuid::Uuid::new_v4(),
            size: Cell::new(Size {
                width: px(412.),
                height: px(915.),
            }),
        }
    }

    pub(crate) fn update_window_bounds(
        &self,
        physical_size: Size<DevicePixels>,
        scale: f32,
        window_bounds: &mut Bounds<Pixels>,
    ) -> bool {
        let logical_size = Size {
            width: px(physical_size.width.0 as f32 / scale),
            height: px(physical_size.height.0 as f32 / scale),
        };
        let changed = window_bounds.size != logical_size;
        window_bounds.size = logical_size;
        self.size.set(logical_size);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_and_density_changes_update_window_and_display_together() {
        let display = AndroidDisplay::new();
        let mut bounds = Bounds::default();
        for (width, height, scale, expected_width, expected_height) in [
            (1080, 1920, 3.0, 360.0, 640.0),
            (1920, 1080, 3.0, 640.0, 360.0),
            (1920, 1080, 2.0, 960.0, 540.0),
        ] {
            assert!(display.update_window_bounds(
                Size {
                    width: DevicePixels(width),
                    height: DevicePixels(height)
                },
                scale,
                &mut bounds,
            ));
            assert_eq!(bounds, display.bounds());
            assert_eq!(bounds.size.width, px(expected_width));
            assert_eq!(bounds.size.height, px(expected_height));
        }
        assert!(!display.update_window_bounds(
            Size {
                width: DevicePixels(1920),
                height: DevicePixels(1080)
            },
            2.0,
            &mut bounds,
        ));
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: self.size.get(),
        }
    }
}
