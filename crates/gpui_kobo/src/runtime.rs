use std::env;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::{
    ButtonDevice, ButtonEvent, Gesture, GestureTracker, MappedTouch, ScreenGeometry, TouchCalibration,
    TouchDevice, TouchTransform, CANVAS_HEIGHT, CANVAS_WIDTH,
};

#[derive(Clone, Copy, Debug)]
pub enum RuntimeEvent {
    Touch { mapped: MappedTouch, gesture: Option<Gesture> },
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
}

impl KoboRuntime {
    pub fn open(geometry: &ScreenGeometry) -> Result<Self> {
        let touch = TouchDevice::discover()?;
        let buttons = ButtonDevice::discover(touch.path())?;
        let inferred = touch.inferred_transform(geometry.view_width, geometry.view_height);
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

    pub fn update_geometry(&mut self, geometry: &ScreenGeometry) {
        if !self.explicit_transform {
            self.transform = self.touch.inferred_transform(geometry.view_width, geometry.view_height);
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
                CANVAS_WIDTH,
                CANVAS_HEIGHT,
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
}
