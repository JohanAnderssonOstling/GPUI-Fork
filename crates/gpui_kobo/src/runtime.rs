use std::env;
use std::os::fd::RawFd;
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
    profile: crate::KoboInputProfile,
    rotation: u8,
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
        Self::open_with_profile(geometry, canvas_width, canvas_height, crate::KoboInputProfile::default())
    }

    pub fn open_with_profile(geometry: &ScreenGeometry, canvas_width: u32, canvas_height: u32, profile: crate::KoboInputProfile) -> Result<Self> {
        let mut touch = TouchDevice::discover()?;
        touch.set_protocol(profile.protocol);
        let buttons = ButtonDevice::discover(touch.path())?;
        let inferred = profile.touch_transforms.map(|transforms| transforms[usize::from(geometry.current_rotation % 4)]).unwrap_or_else(|| orient_touch_transform(
            touch.inferred_transform(geometry.view_width, geometry.view_height),
            geometry.current_rotation,
        ));
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
            profile,
            rotation: geometry.current_rotation % 4,
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
        self.rotation = geometry.current_rotation % 4;
        if !self.explicit_transform {
            self.transform = self.profile.touch_transforms.map(|transforms| transforms[usize::from(self.rotation)]).unwrap_or_else(|| orient_touch_transform(
                self.touch
                    .inferred_transform(geometry.view_width, geometry.view_height),
                geometry.current_rotation,
            ));
        }
    }

    /// Sleep until touch, a hardware key, a dispatched task, or a policy deadline.
    pub(crate) fn wait_for_event(&mut self, timeout: Duration, wake_fd: RawFd) -> Result<RuntimeEvent> {
        let pending = self.next_event(Duration::ZERO)?;
        if !matches!(pending, RuntimeEvent::Idle) { return Ok(pending); }
        let mut descriptors = std::iter::once(self.touch.raw_fd())
            .chain(self.buttons.raw_fds())
            .chain(std::iter::once(wake_fd))
            .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
            .collect::<Vec<_>>();
        crate::input::poll_input(&mut descriptors, timeout)?;
        self.next_event(Duration::ZERO)
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<RuntimeEvent> {
        if let Some(button) = self.buttons.next_event(Duration::ZERO)? {
            return Ok(RuntimeEvent::Button(self.map_button(button)));
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
            return Ok(RuntimeEvent::Button(self.map_button(button)));
        }
        Ok(RuntimeEvent::Idle)
    }

    fn map_button(&self, mut event: ButtonEvent) -> ButtonEvent {
        if let crate::HardwareButton::RotateScreen { rotation, from_gyro: true } = event.button {
            if let Some(rotations) = self.profile.gyroscope_rotations {
                // The generic decoder applies its historical offset first.
                let sensor = rotation.wrapping_add(4 - crate::GYROSCOPE_ROTATION_OFFSET) % 4;
                event.button = crate::HardwareButton::RotateScreen { rotation: rotations[usize::from(sensor)], from_gyro: true };
            }
        } else if self.profile.invert_page_buttons[usize::from(self.rotation)] {
            event.button = match event.button {
                crate::HardwareButton::PreviousPage => crate::HardwareButton::NextPage,
                crate::HardwareButton::NextPage => crate::HardwareButton::PreviousPage,
                other => other,
            };
        }
        event
    }

    pub fn discard_pending_input(&mut self) -> Result<(usize, usize)> {
        let touch_events = self.touch.discard_pending_events()?;
        let button_events = self.buttons.discard_pending_events()?;
        self.gestures = GestureTracker::default();
        Ok((touch_events, button_events))
    }

    /// Keep power-button releases queued so the wake guard can consume them.
    pub fn discard_pending_touch(&mut self) -> Result<usize> {
        self.gestures = GestureTracker::default();
        self.touch.discard_pending_events()
    }

    pub fn next_sleep_button(&mut self, timeout: Duration) -> Result<Option<ButtonEvent>> {
        self.buttons.next_event(timeout)
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
