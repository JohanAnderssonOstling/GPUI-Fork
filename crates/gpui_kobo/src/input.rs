use std::collections::VecDeque;
use std::fs::{self, File};
use std::mem::{self, MaybeUninit};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_MSC: u16 = 0x04;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const SYN_DROPPED: u16 = 0x03;
const BTN_TOUCH: u16 = 330;
const MSC_RAW: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;
const KEY_ROTATE_DISPLAY: u16 = 153;

const MSC_RAW_GSENSOR_PORTRAIT_DOWN: i32 = 0x17;
const MSC_RAW_GSENSOR_PORTRAIT_UP: i32 = 0x18;
const MSC_RAW_GSENSOR_LANDSCAPE_RIGHT: i32 = 0x19;
const MSC_RAW_GSENSOR_LANDSCAPE_LEFT: i32 = 0x1a;

const GYROSCOPE_ROTATIONS: [i32; 4] = [
    MSC_RAW_GSENSOR_LANDSCAPE_LEFT,
    MSC_RAW_GSENSOR_PORTRAIT_UP,
    MSC_RAW_GSENSOR_LANDSCAPE_RIGHT,
    MSC_RAW_GSENSOR_PORTRAIT_DOWN,
];

pub const GYROSCOPE_ROTATION_OFFSET: u8 = 3;

fn adjusted_rotation(rotation: u8) -> u8 {
    (rotation.wrapping_add(GYROSCOPE_ROTATION_OFFSET)) % 4
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawInputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

const INPUT_BATCH_SIZE: usize = 32;

fn read_event_batch(file: &File, operation: &'static str) -> Result<Vec<RawInputEvent>> {
    let mut events = [MaybeUninit::<RawInputEvent>::uninit(); INPUT_BATCH_SIZE];
    let capacity = mem::size_of_val(&events);
    let bytes_read = unsafe {
        libc::read(
            file.as_raw_fd(),
            events.as_mut_ptr().cast::<libc::c_void>(),
            capacity,
        )
    };
    if bytes_read < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| operation);
    }
    let bytes_read = bytes_read as usize;
    let event_size = mem::size_of::<RawInputEvent>();
    if bytes_read == 0 || bytes_read % event_size != 0 {
        bail!("{operation}: incomplete evdev batch of {bytes_read} bytes");
    }
    let count = bytes_read / event_size;
    Ok(events[..count]
        .iter()
        .map(|event| unsafe { (*event).assume_init() })
        .collect())
}

fn discard_ready_events(file: &File, operation: &'static str) -> Result<usize> {
    let mut discarded = 0;
    loop {
        let mut descriptor = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result == 0 || descriptor.revents & libc::POLLIN == 0 {
            return Ok(discarded);
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).with_context(|| operation);
        }
        discarded += read_event_batch(file, operation)?.len();
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RawAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct AxisInfo {
    pub code: u16,
    pub minimum: i32,
    pub maximum: i32,
}

impl AxisInfo {
    fn normalize(self, value: i32) -> f32 {
        let span = (self.maximum - self.minimum).max(1) as f32;
        ((value - self.minimum) as f32 / span).clamp(0.0, 1.0)
    }

    fn span(self) -> i32 {
        (self.maximum - self.minimum).abs()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug)]
pub struct TouchEvent {
    pub phase: TouchPhase,
    pub raw_x: i32,
    pub raw_y: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct MappedTouch {
    pub phase: TouchPhase,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TouchTransform {
    pub swap_axes: bool,
    pub invert_x: bool,
    pub invert_y: bool,
}

impl TouchTransform {
    pub fn parse(value: &str) -> Result<Self> {
        let mut transform = Self::default();
        for token in value
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            match token {
                "none" => transform = Self::default(),
                "swap" => transform.swap_axes = true,
                "invert-x" => transform.invert_x = true,
                "invert-y" => transform.invert_y = true,
                unknown => bail!("unknown touch transform `{unknown}`"),
            }
        }
        Ok(transform)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
    Tap { x: f32, y: f32 },
    LongPress { x: f32, y: f32 },
    Swipe { dx: f32, dy: f32 },
    Cancelled,
}

#[derive(Debug)]
pub struct GestureTracker {
    start: Option<(f32, f32)>,
    last: Option<(f32, f32)>,
    threshold: f32,
    started_at: Option<Instant>,
    long_press_after: Duration,
}

impl Default for GestureTracker {
    fn default() -> Self {
        Self::new(24.0)
    }
}

impl GestureTracker {
    pub fn new(threshold: f32) -> Self {
        Self {
            start: None,
            last: None,
            threshold,
            started_at: None,
            long_press_after: Duration::from_millis(650),
        }
    }

    pub fn observe(&mut self, event: MappedTouch) -> Option<Gesture> {
        self.observe_at(event, Instant::now())
    }

    pub fn observe_at(&mut self, event: MappedTouch, now: Instant) -> Option<Gesture> {
        let point = (event.x, event.y);
        match event.phase {
            TouchPhase::Down => {
                self.start = Some(point);
                self.last = Some(point);
                self.started_at = Some(now);
                None
            }
            TouchPhase::Move => {
                self.last = Some(point);
                None
            }
            TouchPhase::Up => {
                let start = self.start.take().unwrap_or(point);
                self.last.take();
                let held_for = self
                    .started_at
                    .take()
                    .map(|started| now.saturating_duration_since(started));
                let end = point;
                let dx = end.0 - start.0;
                let dy = end.1 - start.1;
                if dx.hypot(dy) >= self.threshold {
                    Some(Gesture::Swipe { dx, dy })
                } else if held_for.is_some_and(|duration| duration >= self.long_press_after) {
                    Some(Gesture::LongPress {
                        x: point.0,
                        y: point.1,
                    })
                } else {
                    Some(Gesture::Tap {
                        x: point.0,
                        y: point.1,
                    })
                }
            }
            TouchPhase::Cancel => {
                self.start = None;
                self.last = None;
                self.started_at = None;
                Some(Gesture::Cancelled)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TouchCalibration {
    pub x_minimum: i32,
    pub x_maximum: i32,
    pub y_minimum: i32,
    pub y_maximum: i32,
}

impl TouchCalibration {
    pub fn parse(value: &str) -> Result<Self> {
        let values: Vec<i32> = value
            .split(',')
            .map(str::trim)
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .context("touch calibration must contain four integers")?;
        if values.len() != 4 || values[1] <= values[0] || values[3] <= values[2] {
            bail!("touch calibration must be x_min,x_max,y_min,y_max with increasing ranges");
        }
        Ok(Self {
            x_minimum: values[0],
            x_maximum: values[1],
            y_minimum: values[2],
            y_maximum: values[3],
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareButton {
    PreviousPage,
    NextPage,
    RotateScreen { rotation: u8, from_gyro: bool },
    Home,
    Power,
    Sleep,
    Unknown(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ButtonEvent {
    pub button: HardwareButton,
    pub pressed: bool,
    pub repeated: bool,
}

pub struct TouchDevice {
    file: File,
    path: PathBuf,
    x_axis: AxisInfo,
    y_axis: AxisInfo,
    raw_x: i32,
    raw_y: i32,
    active: bool,
    reported_active: bool,
    moved: bool,
    lifecycle_source: Option<TouchLifecycleSource>,
    pending_events: VecDeque<RawInputEvent>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TouchLifecycleSource {
    TouchMajor,
    Pressure,
    Button,
    TrackingId,
}

impl TouchDevice {
    pub fn discover() -> Result<Self> {
        let mut paths: Vec<_> = fs::read_dir("/dev/input")
            .context("cannot read /dev/input")?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("event"))
            })
            .collect();
        paths.sort();

        for path in paths {
            if let Some(device) = Self::open_if_touchscreen(&path)? {
                return Ok(device);
            }
        }
        bail!("no evdev touchscreen found under /dev/input")
    }

    fn open_if_touchscreen(path: &Path) -> Result<Option<Self>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };
        let fd = file.as_raw_fd();
        let axes = axis_pair(fd, ABS_MT_POSITION_X, ABS_MT_POSITION_Y)
            .or_else(|| axis_pair(fd, ABS_X, ABS_Y));
        let Some((x_axis, y_axis)) = axes else {
            return Ok(None);
        };

        Ok(Some(Self {
            file,
            path: path.to_path_buf(),
            x_axis,
            y_axis,
            raw_x: x_axis.minimum,
            raw_y: y_axis.minimum,
            active: false,
            reported_active: false,
            moved: false,
            lifecycle_source: None,
            pending_events: VecDeque::new(),
        }))
    }

    pub fn description(&self) -> String {
        format!(
            "{} x=0x{:02x}[{}..{}] y=0x{:02x}[{}..{}]",
            self.path.display(),
            self.x_axis.code,
            self.x_axis.minimum,
            self.x_axis.maximum,
            self.y_axis.code,
            self.y_axis.minimum,
            self.y_axis.maximum,
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn calibration(&self) -> TouchCalibration {
        TouchCalibration {
            x_minimum: self.x_axis.minimum,
            x_maximum: self.x_axis.maximum,
            y_minimum: self.y_axis.minimum,
            y_maximum: self.y_axis.maximum,
        }
    }

    pub fn map_to_canvas(
        &self,
        event: TouchEvent,
        viewport_width: u32,
        viewport_height: u32,
        canvas_width: u32,
        canvas_height: u32,
    ) -> MappedTouch {
        self.map_to_canvas_with_transform(
            event,
            canvas_width,
            canvas_height,
            self.inferred_transform(viewport_width, viewport_height),
        )
    }

    pub fn inferred_transform(&self, viewport_width: u32, viewport_height: u32) -> TouchTransform {
        TouchTransform {
            swap_axes: (self.x_axis.span() > self.y_axis.span())
                != (viewport_width > viewport_height),
            ..TouchTransform::default()
        }
    }

    pub fn map_to_canvas_with_transform(
        &self,
        event: TouchEvent,
        canvas_width: u32,
        canvas_height: u32,
        transform: TouchTransform,
    ) -> MappedTouch {
        self.map_to_canvas_calibrated(
            event,
            canvas_width,
            canvas_height,
            transform,
            self.calibration(),
        )
    }

    pub fn map_to_canvas_calibrated(
        &self,
        event: TouchEvent,
        canvas_width: u32,
        canvas_height: u32,
        transform: TouchTransform,
        calibration: TouchCalibration,
    ) -> MappedTouch {
        let x_axis = AxisInfo {
            code: self.x_axis.code,
            minimum: calibration.x_minimum,
            maximum: calibration.x_maximum,
        };
        let y_axis = AxisInfo {
            code: self.y_axis.code,
            minimum: calibration.y_minimum,
            maximum: calibration.y_maximum,
        };
        let mut x = x_axis.normalize(event.raw_x);
        let mut y = y_axis.normalize(event.raw_y);
        if transform.swap_axes {
            mem::swap(&mut x, &mut y);
        }
        if transform.invert_x {
            x = 1.0 - x;
        }
        if transform.invert_y {
            y = 1.0 - y;
        }
        MappedTouch {
            phase: event.phase,
            x: x * canvas_width.saturating_sub(1) as f32,
            y: y * canvas_height.saturating_sub(1) as f32,
        }
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<TouchEvent>> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(event) = self.pending_events.pop_front() {
                if let Some(event) = self.process_raw_event(event) {
                    return Ok(Some(event));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let mut descriptor = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
            if result == 0 {
                return Ok(None);
            }
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("polling Kobo touchscreen failed");
            }

            self.pending_events.extend(read_event_batch(
                &self.file,
                "reading Kobo touchscreen event batch failed",
            )?);
        }
    }

    pub fn discard_pending_events(&mut self) -> Result<usize> {
        let mut discarded = self.pending_events.len();
        self.pending_events.clear();
        discarded += discard_ready_events(
            &self.file,
            "discarding Kobo touchscreen events after suspend",
        )?;
        self.active = false;
        self.reported_active = false;
        self.moved = false;
        self.lifecycle_source = None;
        Ok(discarded)
    }

    fn process_raw_event(&mut self, event: RawInputEvent) -> Option<TouchEvent> {
        match (event.kind, event.code) {
            (EV_SYN, SYN_DROPPED) => {
                self.active = false;
                self.reported_active = false;
                self.moved = false;
                return Some(TouchEvent {
                    phase: TouchPhase::Cancel,
                    raw_x: self.raw_x,
                    raw_y: self.raw_y,
                });
            }
            (EV_ABS, code) if code == self.x_axis.code => {
                self.raw_x = event.value;
                self.moved = true;
            }
            (EV_ABS, code) if code == self.y_axis.code => {
                self.raw_y = event.value;
                self.moved = true;
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                self.set_active(TouchLifecycleSource::TrackingId, event.value >= 0)
            }
            (EV_KEY, BTN_TOUCH) => self.set_active(TouchLifecycleSource::Button, event.value > 0),
            (EV_ABS, ABS_PRESSURE | ABS_MT_PRESSURE) => {
                self.set_active(TouchLifecycleSource::Pressure, event.value > 0)
            }
            (EV_ABS, ABS_MT_TOUCH_MAJOR) => {
                self.set_active(TouchLifecycleSource::TouchMajor, event.value > 0)
            }
            (EV_SYN, SYN_REPORT) => {
                let phase = if self.active && !self.reported_active {
                    Some(TouchPhase::Down)
                } else if !self.active && self.reported_active {
                    Some(TouchPhase::Up)
                } else if self.active && self.moved {
                    Some(TouchPhase::Move)
                } else {
                    None
                };
                self.reported_active = self.active;
                self.moved = false;
                return phase.map(|phase| TouchEvent {
                    phase,
                    raw_x: self.raw_x,
                    raw_y: self.raw_y,
                });
            }
            _ => {}
        }
        None
    }

    fn set_active(&mut self, source: TouchLifecycleSource, active: bool) {
        let source_changed = self.lifecycle_source != Some(source);
        let accepted = self
            .lifecycle_source
            .map_or(true, |current| source >= current);
        if accepted {
            if crate::verbose_logging_enabled() && (source_changed || self.active != active) {
                println!(
                    "touch lifecycle: source={source:?} active={active} raw=({}, {})",
                    self.raw_x, self.raw_y
                );
            }
            self.lifecycle_source = Some(source);
            self.active = active;
        }
    }
}

struct ButtonSource {
    file: File,
    path: PathBuf,
    pending_events: VecDeque<RawInputEvent>,
}

pub struct ButtonDevice {
    sources: Vec<ButtonSource>,
}

impl ButtonDevice {
    pub fn discover(excluded: &Path) -> Result<Self> {
        let mut paths: Vec<_> = fs::read_dir("/dev/input")
            .context("cannot read /dev/input")?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path != excluded)
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("event"))
            })
            .collect();
        paths.sort();
        let sources = paths
            .into_iter()
            .filter_map(|path| {
                File::open(&path).ok().map(|file| ButtonSource {
                    file,
                    path,
                    pending_events: VecDeque::new(),
                })
            })
            .collect();
        Ok(Self { sources })
    }

    pub fn description(&self) -> String {
        if self.sources.is_empty() {
            return "no separate hardware-button devices".into();
        }
        self.sources
            .iter()
            .map(|source| source.path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<ButtonEvent>> {
        if self.sources.is_empty() {
            return Ok(None);
        }
        for source in &mut self.sources {
            while let Some(event) = source.pending_events.pop_front() {
                if let Some(event) = button_event_from_raw(event.kind, event.code, event.value) {
                    return Ok(Some(event));
                }
            }
        }
        let mut descriptors: Vec<libc::pollfd> = self
            .sources
            .iter()
            .map(|source| libc::pollfd {
                fd: source.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let result =
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout_ms) };
        if result <= 0 {
            return Ok(None);
        }
        for (index, descriptor) in descriptors.iter().enumerate() {
            if descriptor.revents & libc::POLLIN == 0 {
                continue;
            }
            let events = read_event_batch(
                &self.sources[index].file,
                "reading Kobo hardware-button event batch failed",
            )?;
            self.sources[index].pending_events.extend(events);
            while let Some(event) = self.sources[index].pending_events.pop_front() {
                if let Some(event) = button_event_from_raw(event.kind, event.code, event.value) {
                    return Ok(Some(event));
                }
            }
        }
        Ok(None)
    }

    pub fn discard_pending_events(&mut self) -> Result<usize> {
        let mut discarded = 0;
        for source in &mut self.sources {
            discarded += source.pending_events.len();
            source.pending_events.clear();
            discarded += discard_ready_events(
                &source.file,
                "discarding Kobo hardware-button events after suspend",
            )?;
        }
        Ok(discarded)
    }
}

fn button_event_from_raw(kind: u16, code: u16, value: i32) -> Option<ButtonEvent> {
    match (kind, code) {
        (EV_KEY, KEY_ROTATE_DISPLAY) => Some(ButtonEvent {
            button: HardwareButton::RotateScreen {
                rotation: if value != 0 { u8::MAX } else { 0 },
                from_gyro: false,
            },
            pressed: value != 0,
            repeated: value == 2,
        }),
        (EV_KEY, _) => Some(ButtonEvent {
            button: hardware_button_from_code(code),
            pressed: value != 0,
            repeated: value == 2,
        }),
        (EV_MSC, MSC_RAW) => gyroscope_rotation(value).map(|rotation| ButtonEvent {
            button: HardwareButton::RotateScreen {
                rotation,
                from_gyro: true,
            },
            pressed: true,
            repeated: false,
        }),
        _ => None,
    }
}

pub fn hardware_button_from_code(code: u16) -> HardwareButton {
    match code {
        104 | 105 | 193 => HardwareButton::PreviousPage,
        106 | 109 | 194 => HardwareButton::NextPage,
        102 => HardwareButton::Home,
        116 => HardwareButton::Power,
        142 => HardwareButton::Sleep,
        other => HardwareButton::Unknown(other),
    }
}

fn gyroscope_rotation(value: i32) -> Option<u8> {
    GYROSCOPE_ROTATIONS
        .iter()
        .position(|candidate| *candidate == value)
        .map(|rotation| adjusted_rotation(rotation as u8))
}

fn axis_pair(fd: libc::c_int, x_code: u16, y_code: u16) -> Option<(AxisInfo, AxisInfo)> {
    let x = abs_info(fd, x_code)?;
    let y = abs_info(fd, y_code)?;
    (x.maximum > x.minimum && y.maximum > y.minimum).then_some((
        AxisInfo {
            code: x_code,
            minimum: x.minimum,
            maximum: x.maximum,
        },
        AxisInfo {
            code: y_code,
            minimum: y.minimum,
            maximum: y.maximum,
        },
    ))
}

fn abs_info(fd: libc::c_int, code: u16) -> Option<RawAbsInfo> {
    let mut info = RawAbsInfo::default();
    let request = ioctl_read_request(b'E', 0x40 + code as u8, mem::size_of::<RawAbsInfo>());
    let result = unsafe { libc::ioctl(fd, request as _, &mut info) };
    (result >= 0).then_some(info)
}

fn ioctl_read_request(kind: u8, number: u8, size: usize) -> libc::c_ulong {
    const IOC_READ: libc::c_ulong = 2;
    const IOC_NRSHIFT: libc::c_ulong = 0;
    const IOC_TYPESHIFT: libc::c_ulong = 8;
    const IOC_SIZESHIFT: libc::c_ulong = 16;
    const IOC_DIRSHIFT: libc::c_ulong = 30;
    (IOC_READ << IOC_DIRSHIFT)
        | ((kind as libc::c_ulong) << IOC_TYPESHIFT)
        | ((number as libc::c_ulong) << IOC_NRSHIFT)
        | ((size as libc::c_ulong) << IOC_SIZESHIFT)
}
