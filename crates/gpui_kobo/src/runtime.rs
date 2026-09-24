use std::env;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::{
    ButtonDevice, ButtonEvent, Gesture, GestureTracker, MappedTouch, ScreenGeometry,
    TouchCalibration, TouchDevice, TouchTransform,
};

#[derive(Clone, Copy, Debug)]
pub enum RuntimeEvent {
    Touch {
        mapped: MappedTouch,
        gesture: Option<Gesture>,
    },
    Button(ButtonEvent),
    Idle,
}

/// Kobo-native event loop boundary around GPUI's retained window. It owns raw
/// input polling, coordinate policy, gesture timing, and geometry changes.
pub struct KoboRuntime {
    touch: TouchDevice,
    buttons: ButtonDevice,
    gestures: GestureTracker,
    transform: TouchTransform,
    calibration: TouchCalibration,
    explicit_transform: bool,
    canvas_width: u32,
    canvas_height: u32,
}

fn orient_touch_transform(mut transform: TouchTransform, rotation: u8) -> TouchTransform {
    // Use the same physical coordinate basis at startup and after rotation.
    // Gyroscope events are adjusted into FBInk rotations; undo that offset for
    // touch regardless of how the current display orientation was selected.
    let touch_rotation_offset = (4 - crate::GYROSCOPE_ROTATION_OFFSET) % 4;
    match (rotation + touch_rotation_offset) % 4 {
        0 => transform.invert_y = !transform.invert_y,
        1 => {}
        2 => transform.invert_x = !transform.invert_x,
        3 => {
            transform.invert_x = !transform.invert_x;
            transform.invert_y = !transform.invert_y;
        }
        _ => unreachable!(),
    }
    transform
}

impl KoboRuntime {
    pub fn open(geometry: &ScreenGeometry, canvas_width: u32, canvas_height: u32) -> Result<Self> {
        let touch = TouchDevice::discover()?;
        let buttons = ButtonDevice::discover(touch.path())?;
        let inferred = orient_touch_transform(
            touch.inferred_transform(geometry.view_width, geometry.view_height),
            geometry.current_rotation,
        );
        let (transform, explicit_transform) = match env::var("GPUI_KOBO_TOUCH_TRANSFORM") {
            Ok(value) => (
                TouchTransform::parse(&value)
                    .with_context(|| format!("invalid GPUI_KOBO_TOUCH_TRANSFORM={value}"))?,
                true,
            ),
            Err(env::VarError::NotPresent) => (inferred, false),
            Err(error) => return Err(error).context("reading GPUI_KOBO_TOUCH_TRANSFORM"),
        };
        let calibration = match env::var("GPUI_KOBO_TOUCH_CALIBRATION") {
            Ok(value) => TouchCalibration::parse(&value)
                .with_context(|| format!("invalid GPUI_KOBO_TOUCH_CALIBRATION={value}"))?,
            Err(env::VarError::NotPresent) => touch.calibration(),
            Err(error) => return Err(error).context("reading GPUI_KOBO_TOUCH_CALIBRATION"),
        };
        Ok(Self {
            touch,
            buttons,
            gestures: GestureTracker::default(),
            transform,
            calibration,
            explicit_transform,
            canvas_width,
            canvas_height,
        })
    }

    pub fn touch_description(&self) -> String {
        self.touch.description()
    }

    pub fn button_description(&self) -> String {
        self.buttons.description()
    }

    pub fn transform(&self) -> TouchTransform {
        self.transform
    }

    pub fn calibration(&self) -> TouchCalibration {
        self.calibration
    }

    pub fn set_canvas_size(&mut self, canvas_width: u32, canvas_height: u32) {
        self.canvas_width = canvas_width.max(1);
        self.canvas_height = canvas_height.max(1);
    }

    pub fn update_geometry(&mut self, geometry: &ScreenGeometry) {
        if !self.explicit_transform {
            self.transform = orient_touch_transform(
                self.touch
                    .inferred_transform(geometry.view_width, geometry.view_height),
                geometry.current_rotation,
            );
        }
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<RuntimeEvent> {
        if let Some(button) = self.buttons.next_event(Duration::ZERO)? {
            return Ok(RuntimeEvent::Button(button));
        }

        let touch_timeout = timeout.min(Duration::from_millis(50));
        if let Some(event) = self.touch.next_event(touch_timeout)? {
            let mapped = self.touch.map_to_canvas_calibrated(
                event,
                self.canvas_width,
                self.canvas_height,
                self.transform,
                self.calibration,
            );
            let gesture = self.gestures.observe(mapped);
            return Ok(RuntimeEvent::Touch { mapped, gesture });
        }

        if let Some(button) = self.buttons.next_event(Duration::ZERO)? {
            return Ok(RuntimeEvent::Button(button));
        }
        Ok(RuntimeEvent::Idle)
    }

    pub fn discard_pending_input(&mut self) -> Result<(usize, usize)> {
        let touch_events = self.touch.discard_pending_events()?;
        let button_events = self.buttons.discard_pending_events()?;
        self.gestures = GestureTracker::default();
        Ok((touch_events, button_events))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_portrait_does_not_flip_taps_to_opposite_controls() {
        let transform = orient_touch_transform(TouchTransform::default(), 0);
        assert_eq!(transform, TouchTransform::default());
    }

    #[test]
    fn all_orientations_preserve_axis_swap_and_use_the_correct_inversions() {
        for swap_axes in [false, true] {
            for (rotation, invert_x, invert_y) in [
                (0, false, false),
                (1, true, false),
                (2, true, true),
                (3, false, true),
            ] {
                let base = TouchTransform {
                    swap_axes,
                    ..TouchTransform::default()
                };
                assert_eq!(
                    orient_touch_transform(base, rotation),
                    TouchTransform {
                        swap_axes,
                        invert_x,
                        invert_y
                    }
                );
            }
        }
    }
}
