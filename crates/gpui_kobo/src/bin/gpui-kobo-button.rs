use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use gpui_kobo::{
    ButtonRenderSession, CANVAS_HEIGHT, CANVAS_WIDTH, FrameUpdate, PixelRect, TouchDevice,
    TouchPhase, damage_image, write_pgm,
};
use image::RgbaImage;

const DEFAULT_OUTPUT: &str = "/tmp/gpui-kobo-library.pgm";
const DEFAULT_TIMEOUT_SECONDS: u64 = 45;

struct Options {
    output: PathBuf,
    fbink: PathBuf,
    display: bool,
    interactive: bool,
    self_test: bool,
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
    if options.self_test {
        return run_self_test();
    }
    if options.interactive && !options.display {
        bail!("--interactive cannot be combined with --no-display");
    }

    let mut session = ButtonRenderSession::new()?;
    let image = session
        .capture()
        .context("GPUI failed to render the initial library scene")?;
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

fn run_self_test() -> Result<()> {
    let started = Instant::now();
    let mut session = ButtonRenderSession::new().context("SELFTEST FAIL session initialization")?;
    let initialized_ms = started.elapsed().as_millis();

    let render_started = Instant::now();
    let initial = session.capture().context("SELFTEST FAIL initial text render")?;
    let initial_render_ms = render_started.elapsed().as_millis();
    let antialiased_pixels = initial
        .pixels()
        .filter(|pixel| !matches!(pixel.0[0], 24 | 224 | 240 | 255))
        .count();
    ensure!(
        antialiased_pixels > 100,
        "SELFTEST FAIL text rasterization produced only {antialiased_pixels} antialiased pixels"
    );
    println!(
        "SELFTEST PASS text_rasterization antialiased_pixels={antialiased_pixels} init_ms={initialized_ms} initial_render_ms={initial_render_ms}"
    );

    let renders_before_drag = session.render_count();
    ensure!(
        session.dispatch_touch(TouchPhase::Down, 300.0, 500.0)?.is_none(),
        "SELFTEST FAIL touch-down produced a frame"
    );
    ensure!(
        session.dispatch_touch(TouchPhase::Move, 300.0, 420.0)?.is_none(),
        "SELFTEST FAIL first touch-move produced a frame"
    );
    ensure!(
        session.dispatch_touch(TouchPhase::Move, 300.0, 340.0)?.is_none(),
        "SELFTEST FAIL second touch-move produced a frame"
    );
    ensure!(
        session.render_count() == renders_before_drag,
        "SELFTEST FAIL drag rendered before finger-up"
    );
    println!("SELFTEST PASS repaint_guard renders_during_drag=0");

    let release_started = Instant::now();
    let update = session
        .dispatch_touch(TouchPhase::Up, 300.0, 300.0)?
        .context("SELFTEST FAIL finger-up produced no changed frame")?;
    let release_ms = release_started.elapsed().as_millis();
    ensure!(
        session.render_count() == renders_before_drag + 1,
        "SELFTEST FAIL finger-up did not produce exactly one render"
    );
    let damage_bottom = update.damage.y + update.damage.height;
    ensure!(
        update.damage.y >= 108 && damage_bottom <= 744,
        "SELFTEST FAIL scroll damage escaped list viewport: x={} y={} width={} height={}",
        update.damage.x,
        update.damage.y,
        update.damage.width,
        update.damage.height
    );
    println!(
        "SELFTEST PASS release_scroll renders=1 release_ms={release_ms} damage_x={} damage_y={} damage_width={} damage_height={}",
        update.damage.x, update.damage.y, update.damage.width, update.damage.height
    );

    let renders_before_exit = session.render_count();
    ensure!(
        session.dispatch_touch(TouchPhase::Up, 510.0, 54.0)?.is_none(),
        "SELFTEST FAIL EXIT produced an unnecessary framebuffer update"
    );
    ensure!(session.state().exit_requested, "SELFTEST FAIL EXIT hit testing");
    ensure!(
        session.render_count() == renders_before_exit,
        "SELFTEST FAIL EXIT caused an unnecessary render"
    );
    println!("SELFTEST PASS exit_hit_test framebuffer_renders=0");
    println!("SELFTEST PASS all total_ms={}", started.elapsed().as_millis());
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
        "viewport: {}x{}; drag the GPUI library list and tap EXIT to return",
        viewport.width, viewport.height
    );

    let damage_path = options.output.with_file_name("gpui-kobo-library-damage.pgm");
    let deadline = Instant::now() + options.timeout;

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

        if let Some(update) = session.dispatch_touch(mapped.phase, mapped.x, mapped.y)? {
            println!(
                "GPUI damage: x={} y={} width={} height={}",
                update.damage.x, update.damage.y, update.damage.width, update.damage.height
            );
            render_damage(&update, &options.fbink, &damage_path, viewport)?;
        }

        if session.state().exit_requested {
            println!("EXIT activated through GPUI hit testing");
            break;
        }
    }
    Ok(())
}

fn render_damage(
    update: &FrameUpdate,
    fbink: &Path,
    damage_path: &Path,
    viewport: Viewport,
) -> Result<()> {
    let damage = damage_image(&update.image, update.damage);
    write_pgm(&damage, damage_path)?;
    present_damage(fbink, damage_path, update.damage, viewport)
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

fn present_damage(
    fbink: &Path,
    image: &Path,
    damage: PixelRect,
    viewport: Viewport,
) -> Result<()> {
    let x = damage.x * viewport.width / CANVAS_WIDTH;
    let y = damage.y * viewport.height / CANVAS_HEIGHT;
    let width = damage.width * viewport.width / CANVAS_WIDTH;
    let height = damage.height * viewport.height / CANVAS_HEIGHT;
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
    let mut self_test = false;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECONDS);
    let mut args = env::args_os().skip(1);

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--output") => output = required_value("--output", args.next())?.into(),
            Some("--fbink") => fbink = required_value("--fbink", args.next())?.into(),
            Some("--no-display") => display = false,
            Some("--interactive") => interactive = true,
            Some("--self-test") => self_test = true,
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
        self_test,
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
        "rendered {}x{} GPUI library to {}",
        image.width(),
        image.height(),
        output.display()
    );
}

fn print_help() {
    println!(
        "gpui-kobo-button [--output PATH] [--fbink PATH] [--no-display]\n\
         \t[--interactive] [--self-test] [--timeout-seconds N]\n\
         \n\
         Render a text library through GPUI's CPU Kobo renderer. Interactive mode\n\
         translates evdev drags into GPUI scrolling and uses pixel-derived FBInk\n\
         damage updates. Tap EXIT to return. The recovery timeout is\n\
         {DEFAULT_TIMEOUT_SECONDS} seconds."
    );
}
