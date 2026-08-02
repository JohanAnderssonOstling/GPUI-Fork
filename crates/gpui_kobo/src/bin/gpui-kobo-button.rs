use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use gpui_kobo::{
    ButtonRenderSession, CANVAS_HEIGHT, CANVAS_WIDTH, FrameUpdate, Gesture, GestureTracker,
    MappedTouch, RefreshMode, RefreshPolicy, ScreenGeometry, TouchDevice, TouchPhase,
    TouchTransform, damage_image, rgba8_to_grayscale, write_pgm,
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

    let geometry = query_geometry(&options.fbink)?;
    println!("display: {}", geometry.description());
    present_full(&options.fbink, &options.output)?;
    print_render_result(&image, &options.output);

    if options.interactive {
        run_interactive(&options, geometry, &mut session)?;
    }
    Ok(())
}

fn run_self_test() -> Result<()> {
    let started = Instant::now();

    let geometry = ScreenGeometry::from_fbink_state(
        "viewWidth=600;viewHeight=800;screenWidth=758;screenHeight=1024;\
         viewHoriOrigin=0;viewVertOrigin=42;currentRota=1;\
         deviceName='Kobo Test';deviceCodename='kraken';",
    )
    .context("SELFTEST FAIL FBInk geometry parser")?;
    ensure!(
        geometry.view_width == 600
            && geometry.view_height == 800
            && geometry.view_y == 42
            && geometry.current_rotation == 1,
        "SELFTEST FAIL FBInk geometry values"
    );
    let scaled = geometry.scale_damage(
        gpui_kobo::PixelRect { x: 10, y: 20, width: 101, height: 51 },
        300,
        400,
    );
    ensure!(
        scaled.x == 20 && scaled.y == 40 && scaled.width == 202 && scaled.height == 102,
        "SELFTEST FAIL damage scaling produced {scaled:?}"
    );
    println!("SELFTEST PASS display_geometry {}", geometry.description());

    let transform = TouchTransform::parse("swap,invert-x")
        .context("SELFTEST FAIL touch transform parser")?;
    ensure!(
        transform.swap_axes && transform.invert_x && !transform.invert_y,
        "SELFTEST FAIL touch transform values"
    );
    let mut gestures = GestureTracker::default();
    gestures.observe(MappedTouch { phase: TouchPhase::Down, x: 300.0, y: 500.0 });
    gestures.observe(MappedTouch { phase: TouchPhase::Move, x: 300.0, y: 400.0 });
    let gesture = gestures.observe(MappedTouch {
        phase: TouchPhase::Up,
        x: 300.0,
        y: 300.0,
    });
    ensure!(
        matches!(gesture, Some(Gesture::Swipe { dx, dy }) if dx == 0.0 && dy == -200.0),
        "SELFTEST FAIL swipe classification: {gesture:?}"
    );
    println!("SELFTEST PASS touch_transform_and_gesture gesture={gesture:?}");

    let policy_damage = gpui_kobo::PixelRect { x: 0, y: 100, width: 100, height: 100 };
    let mut policy = RefreshPolicy::default();
    ensure!(
        policy.decide(false, policy_damage, 480_000) == RefreshMode::PartialGray,
        "SELFTEST FAIL tap refresh mode"
    );
    for update_number in 1..6 {
        ensure!(
            policy.decide(true, policy_damage, 480_000) == RefreshMode::FastMono,
            "SELFTEST FAIL fast refresh {update_number}"
        );
    }
    ensure!(
        policy.decide(true, policy_damage, 480_000) == RefreshMode::FullGray,
        "SELFTEST FAIL ghosting cleanup refresh"
    );
    ensure!(policy.fast_updates() == 0, "SELFTEST FAIL cleanup counter reset");
    let area_damage = gpui_kobo::PixelRect { x: 0, y: 100, width: 600, height: 600 };
    let mut area_policy = RefreshPolicy::default();
    for update_number in 1..4 {
        ensure!(
            area_policy.decide(true, area_damage, 480_000) == RefreshMode::FastMono,
            "SELFTEST FAIL area fast refresh {update_number}"
        );
    }
    ensure!(
        area_policy.decide(true, area_damage, 480_000) == RefreshMode::FullGray,
        "SELFTEST FAIL accumulated-area cleanup refresh"
    );
    println!("SELFTEST PASS refresh_policy count_cleanup=6 area_cleanup=3_screens");

    let (red_gray, red_alpha) = rgba8_to_grayscale([255, 0, 0, 128], 0.5);
    let (white_gray, white_alpha) = rgba8_to_grayscale([255, 255, 255, 255], 1.0);
    ensure!(
        red_gray == 54 && (red_alpha - 0.250_98).abs() < 0.001,
        "SELFTEST FAIL RGBA sprite conversion red=({red_gray}, {red_alpha})"
    );
    ensure!(
        white_gray == 255 && white_alpha == 1.0,
        "SELFTEST FAIL RGBA sprite conversion white=({white_gray}, {white_alpha})"
    );
    println!("SELFTEST PASS polychrome_sprite grayscale=54 alpha={red_alpha:.3}");

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
    geometry: ScreenGeometry,
    session: &mut ButtonRenderSession,
) -> Result<()> {
    let mut touch = TouchDevice::discover()?;
    let inferred_transform = touch.inferred_transform(geometry.view_width, geometry.view_height);
    let transform = match env::var("GPUI_KOBO_TOUCH_TRANSFORM") {
        Ok(value) => TouchTransform::parse(&value)
            .with_context(|| format!("invalid GPUI_KOBO_TOUCH_TRANSFORM={value}"))?,
        Err(env::VarError::NotPresent) => inferred_transform,
        Err(error) => return Err(error).context("reading GPUI_KOBO_TOUCH_TRANSFORM"),
    };
    println!("touchscreen: {}", touch.description());
    println!(
        "touch transform: swap={} invert_x={} invert_y={}",
        transform.swap_axes, transform.invert_x, transform.invert_y
    );
    println!("drag the GPUI library list and tap EXIT to return");

    let damage_path = options.output.with_file_name("gpui-kobo-library-damage.pgm");
    let deadline = Instant::now() + options.timeout;
    let mut gestures = GestureTracker::default();
    let mut refresh_policy = RefreshPolicy::default();

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
        let mapped = touch.map_to_canvas_with_transform(
            event,
            CANVAS_WIDTH,
            CANVAS_HEIGHT,
            transform,
        );
        let gesture = gestures.observe(mapped);

        if let Some(update) = session.dispatch_touch(mapped.phase, mapped.x, mapped.y)? {
            let scrolling = matches!(gesture, Some(Gesture::Swipe { .. }));
            let mode = refresh_policy.decide(
                scrolling,
                update.damage,
                u64::from(CANVAS_WIDTH) * u64::from(CANVAS_HEIGHT),
            );
            println!(
                "GPUI release: gesture={gesture:?} refresh={mode:?} damage=x:{},y:{},w:{},h:{}",
                update.damage.x, update.damage.y, update.damage.width, update.damage.height,
            );
            render_update(
                &update,
                mode,
                &options.fbink,
                &options.output,
                &damage_path,
                &geometry,
            )?;
        }

        if session.state().exit_requested {
            println!("EXIT activated through GPUI hit testing");
            break;
        }
    }
    Ok(())
}

fn render_update(
    update: &FrameUpdate,
    mode: RefreshMode,
    fbink: &Path,
    full_path: &Path,
    damage_path: &Path,
    geometry: &ScreenGeometry,
) -> Result<()> {
    if mode == RefreshMode::FullGray {
        write_pgm(&update.image, full_path)?;
        return present_full(fbink, full_path);
    }
    let damage = damage_image(&update.image, update.damage);
    write_pgm(&damage, damage_path)?;
    present_damage(fbink, damage_path, update.damage, geometry, mode)
}

fn query_geometry(fbink: &Path) -> Result<ScreenGeometry> {
    let output = Command::new(fbink)
        .args(["-q", "-e"])
        .output()
        .with_context(|| format!("failed to query {}", fbink.display()))?;
    if !output.status.success() {
        bail!("FBInk state query exited with {}", output.status);
    }
    let state = String::from_utf8(output.stdout).context("FBInk state was not UTF-8")?;
    ScreenGeometry::from_fbink_state(&state)
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
    damage: gpui_kobo::PixelRect,
    geometry: &ScreenGeometry,
    mode: RefreshMode,
) -> Result<()> {
    let damage = geometry.scale_damage(damage, CANVAS_WIDTH, CANVAS_HEIGHT);
    run_fbink(
        fbink,
        image,
        &["-q", "-w", "-W", mode.waveform()],
        format!(
            "x={},y={},w={},h={}",
            damage.x, damage.y, damage.width, damage.height
        ),
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
