use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use gpui_kobo::{
    BUTTON_DAMAGE, ButtonRenderSession, CANVAS_HEIGHT, CANVAS_WIDTH, TouchDevice, TouchPhase,
    button_damage_image, write_pgm,
};
use image::RgbaImage;

const DEFAULT_OUTPUT: &str = "/tmp/gpui-kobo-button.pgm";
const DEFAULT_TIMEOUT_SECONDS: u64 = 45;

struct Options {
    output: PathBuf,
    fbink: PathBuf,
    display: bool,
    interactive: bool,
    timeout: Duration,
}

#[derive(Clone, Copy)]
struct Viewport {
    width: u32,
    height: u32,
}

fn main() -> Result<()> {
    let Some(options) = parse_options()? else {
        return Ok(());
    };
    if options.interactive && !options.display {
        bail!("--interactive cannot be combined with --no-display");
    }

    let mut session = ButtonRenderSession::new()?;
    let image = session
        .capture()
        .context("GPUI failed to render the initial button scene")?;
    write_pgm(&image, &options.output)
        .with_context(|| format!("failed to write {}", options.output.display()))?;

    if !options.display {
        print_render_result(&image, &options.output);
        return Ok(());
    }

    let viewport = query_viewport(&options.fbink)?;
    present_full(&options.fbink, &options.output)?;
    print_render_result(&image, &options.output);

    if options.interactive {
        run_interactive(&options, viewport, &mut session)?;
    }
    Ok(())
}

fn run_interactive(
    options: &Options,
    viewport: Viewport,
    session: &mut ButtonRenderSession,
) -> Result<()> {
    let mut touch = TouchDevice::discover()?;
    println!("touchscreen: {}", touch.description());
    println!(
        "viewport: {}x{}; tap the GPUI button once to activate it and again to exit",
        viewport.width, viewport.height
    );

    let damage_path = options.output.with_file_name("gpui-kobo-button-damage.pgm");
    let deadline = Instant::now() + options.timeout;
    let mut pressed_inside = false;
    let mut activated = false;
    let mut completed_taps = 0_u8;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            println!("interactive timeout reached");
            break;
        }
        let Some(event) = touch.next_event(remaining)? else {
            println!("interactive timeout reached");
            break;
        };
        let mapped = touch.map_to_canvas(
            event,
            viewport.width,
            viewport.height,
            CANVAS_WIDTH,
            CANVAS_HEIGHT,
        );
        println!(
            "touch {:?}: raw=({}, {}) canvas=({:.1}, {:.1})",
            mapped.phase, event.raw_x, event.raw_y, mapped.x, mapped.y
        );

        match mapped.phase {
            TouchPhase::Down if BUTTON_DAMAGE.contains(mapped.x, mapped.y) => {
                pressed_inside = true;
                render_damage(
                    session,
                    true,
                    activated,
                    &options.fbink,
                    &damage_path,
                    viewport,
                )?;
            }
            TouchPhase::Up if pressed_inside => {
                pressed_inside = false;
                completed_taps = completed_taps.saturating_add(1);
                if completed_taps == 1 {
                    activated = true;
                    render_damage(
                        session,
                        false,
                        true,
                        &options.fbink,
                        &damage_path,
                        viewport,
                    )?;
                    println!("button activated; tap it again to exit");
                } else {
                    render_damage(
                        session,
                        false,
                        false,
                        &options.fbink,
                        &damage_path,
                        viewport,
                    )?;
                    println!("second button tap received; exiting");
                    thread::sleep(Duration::from_millis(350));
                    break;
                }
            }
            TouchPhase::Up => pressed_inside = false,
            _ => {}
        }
    }
    Ok(())
}

fn render_damage(
    session: &mut ButtonRenderSession,
    pressed: bool,
    activated: bool,
    fbink: &Path,
    damage_path: &Path,
    viewport: Viewport,
) -> Result<()> {
    let image = session.set_state(pressed, activated)?;
    let damage = button_damage_image(&image);
    write_pgm(&damage, damage_path)?;
    present_damage(fbink, damage_path, viewport)
}

fn query_viewport(fbink: &Path) -> Result<Viewport> {
    let output = Command::new(fbink)
        .args(["-q", "-e"])
        .output()
        .with_context(|| format!("failed to query {}", fbink.display()))?;
    if !output.status.success() {
        bail!("FBInk state query exited with {}", output.status);
    }
    let state = String::from_utf8(output.stdout).context("FBInk state was not UTF-8")?;
    Ok(Viewport {
        width: state_value(&state, "viewWidth")?,
        height: state_value(&state, "viewHeight")?,
    })
}

fn state_value(state: &str, name: &str) -> Result<u32> {
    state
        .split(';')
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .ok_or_else(|| anyhow::anyhow!("FBInk state omitted {name}"))?
        .parse()
        .with_context(|| format!("FBInk returned an invalid {name}"))
}

fn present_full(fbink: &Path, image: &Path) -> Result<()> {
    run_fbink(
        fbink,
        image,
        &["-q", "-c", "-f", "-w", "-W", "GC16"],
        "halign=CENTER,valign=CENTER,w=-1,h=-1".to_string(),
    )
}

fn present_damage(fbink: &Path, image: &Path, viewport: Viewport) -> Result<()> {
    let x = BUTTON_DAMAGE.x * viewport.width / CANVAS_WIDTH;
    let y = BUTTON_DAMAGE.y * viewport.height / CANVAS_HEIGHT;
    let width = BUTTON_DAMAGE.width * viewport.width / CANVAS_WIDTH;
    let height = BUTTON_DAMAGE.height * viewport.height / CANVAS_HEIGHT;
    run_fbink(
        fbink,
        image,
        &["-q", "-w", "-W", "A2"],
        format!("x={x},y={y},w={width},h={height}"),
    )
}

fn run_fbink(fbink: &Path, image: &Path, flags: &[&str], image_options: String) -> Result<()> {
    let status = Command::new(fbink)
        .args(flags)
        .arg("-i")
        .arg(image)
        .args(["-g", &image_options])
        .status()
        .with_context(|| format!("failed to start {}", fbink.display()))?;
    if !status.success() {
        bail!("FBInk exited with {status}");
    }
    Ok(())
}

fn parse_options() -> Result<Option<Options>> {
    let mut output = PathBuf::from(DEFAULT_OUTPUT);
    let mut fbink = default_fbink_path();
    let mut display = true;
    let mut interactive = false;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECONDS);
    let mut args = env::args_os().skip(1);

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--output") => output = required_value("--output", args.next())?.into(),
            Some("--fbink") => fbink = required_value("--fbink", args.next())?.into(),
            Some("--no-display") => display = false,
            Some("--interactive") => interactive = true,
            Some("--timeout-seconds") => {
                let value = required_value("--timeout-seconds", args.next())?;
                let seconds = value
                    .to_string_lossy()
                    .parse()
                    .context("--timeout-seconds must be an integer")?;
                timeout = Duration::from_secs(seconds);
            }
            Some("-h" | "--help") => {
                print_help();
                return Ok(None);
            }
            _ => bail!("unknown argument: {}", arg.to_string_lossy()),
        }
    }

    Ok(Some(Options {
        output,
        fbink,
        display,
        interactive,
        timeout,
    }))
}

fn required_value(name: &str, value: Option<OsString>) -> Result<OsString> {
    value.ok_or_else(|| anyhow::anyhow!("{name} requires a value"))
}

fn default_fbink_path() -> PathBuf {
    if let Some(path) = env::var_os("FBINK_BIN") {
        return path.into();
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let sibling = directory.join("fbink");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("fbink")
}

fn print_render_result(image: &RgbaImage, output: &Path) {
    println!(
        "rendered {}x{} GPUI button to {}",
        image.width(),
        image.height(),
        output.display()
    );
}

fn print_help() {
    println!(
        "gpui-kobo-button [--output PATH] [--fbink PATH] [--no-display]\n\
         	[--interactive] [--timeout-seconds N]\n\
         \n\
         Render one button through GPUI's CPU Kobo renderer. Interactive mode\n\
         reads evdev touch input: the first button tap activates it and the\n\
         second exits. The default recovery timeout is {DEFAULT_TIMEOUT_SECONDS} seconds."
    );
}
