use std::fs::{self, File};
use std::io::Read;
use std::mem::{self, MaybeUninit};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::slice;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 330;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;

#[repr(C)]
#[derive(Clone, Copy)]
struct RawInputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
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

    pub fn map_to_canvas(
        &self,
        event: TouchEvent,
        viewport_width: u32,
        viewport_height: u32,
        canvas_width: u32,
        canvas_height: u32,
    ) -> MappedTouch {
        let mut x = self.x_axis.normalize(event.raw_x);
        let mut y = self.y_axis.normalize(event.raw_y);
        let raw_is_landscape = self.x_axis.span() > self.y_axis.span();
        let viewport_is_landscape = viewport_width > viewport_height;
        if raw_is_landscape != viewport_is_landscape {
            mem::swap(&mut x, &mut y);
        }
        MappedTouch {
            phase: event.phase,
            x: x * canvas_width as f32,
            y: y * canvas_height as f32,
        }
    }

    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<TouchEvent>> {
        let deadline = Instant::now() + timeout;
        loop {
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

            let mut event = MaybeUninit::<RawInputEvent>::uninit();
            let bytes = unsafe {
                slice::from_raw_parts_mut(
                    event.as_mut_ptr().cast::<u8>(),
                    mem::size_of::<RawInputEvent>(),
                )
            };
            self.file
                .read_exact(bytes)
                .context("reading Kobo touchscreen event failed")?;
            let event = unsafe { event.assume_init() };
            if let Some(event) = self.process_raw_event(event) {
                return Ok(Some(event));
            }
        }
    }

    fn process_raw_event(&mut self, event: RawInputEvent) -> Option<TouchEvent> {
        match (event.kind, event.code) {
            (EV_ABS, code) if code == self.x_axis.code => {
                self.raw_x = event.value;
                self.moved = true;
            }
            (EV_ABS, code) if code == self.y_axis.code => {
                self.raw_y = event.value;
                self.moved = true;
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => self.active = event.value >= 0,
            (EV_ABS, ABS_PRESSURE | ABS_MT_TOUCH_MAJOR | ABS_MT_PRESSURE) => {
                self.active = event.value > 0;
            }
            (EV_KEY, BTN_TOUCH) => self.active = event.value > 0,
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
