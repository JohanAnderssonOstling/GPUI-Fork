use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use gpui::Application;
use gpui_kobo::{
    DeviceRefreshProfile, FrameUpdate, Gesture, GestureTracker, HardwareButton,
    MappedTouch, RepaintScheduler, RefreshMode, RefreshPolicy, RuntimeEvent, ScreenGeometry,
    TouchCalibration, TouchPhase, TouchTransform, extended_renderer_self_test,
    hardware_button_from_code, presenter_self_test, rgba8_to_grayscale, write_pgm,
    KoboAssets, KoboPlatform, KoboPlatformOptions, open_button_test_window,
};
use image::RgbaImage;

const DEFAULT_OUTPUT: &str = "/tmp/gpui-kobo-library.pgm";
const DEFAULT_TIMEOUT_SECONDS: u64 = 45;

struct Options {
    output: PathBuf,
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

    let run = run_platform_application(&options, Vec::new())?;
    let image = run.image;
    if !options.display {
        write_pgm(&image, &options.output)
            .with_context(|| format!("failed to write {}", options.output.display()))?;
        println!("rendered {}x{} GPUI library to {}", image.width(), image.height(), options.output.display());
        return Ok(());
    }

    println!("rendered {}x{} GPUI library through linked libfbink", image.width(), image.height());
    Ok(())
}

struct PlatformRun {
    image: RgbaImage,
    render_count: u64,
    quit_requested: bool,
}

fn run_platform_application(options: &Options, events: Vec<RuntimeEvent>) -> Result<PlatformRun> {
    let platform = KoboPlatform::new(KoboPlatformOptions {
        display: options.display,
        interactive: options.interactive,
        timeout: options.timeout,
        ..Default::default()
    })?;
    for event in events {
        platform.queue_event(event);
    }
    Application::new_inaccessible(platform.clone())
        .with_assets(KoboAssets)
        .run(|cx| open_button_test_window(cx).expect("failed to open Kobo GPUI window"));
    if let Some(error) = platform.take_error() {
        return Err(error).context("Kobo GPUI platform failed");
    }
    let image = platform.last_frame().context("Kobo GPUI platform produced no frame")?;
    Ok(PlatformRun {
        image,
        render_count: platform.render_count(),
        quit_requested: platform.quit_requested(),
    })
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

    let started_at = Instant::now();
    let mut long_press = GestureTracker::default();
    long_press.observe_at(
        MappedTouch { phase: TouchPhase::Down, x: 10.0, y: 20.0 },
        started_at,
    );
    let long_press_gesture = long_press.observe_at(
        MappedTouch { phase: TouchPhase::Up, x: 12.0, y: 21.0 },
        started_at + Duration::from_millis(700),
    );
    ensure!(
        matches!(long_press_gesture, Some(Gesture::LongPress { .. })),
        "SELFTEST FAIL long-press classification: {long_press_gesture:?}"
    );
    let cancelled = long_press.observe(MappedTouch { phase: TouchPhase::Cancel, x: 0.0, y: 0.0 });
    ensure!(cancelled == Some(Gesture::Cancelled), "SELFTEST FAIL touch cancellation");
    let calibration = TouchCalibration::parse("10,1010,20,2020")
        .context("SELFTEST FAIL touch calibration parser")?;
    ensure!(calibration.x_minimum == 10 && calibration.y_maximum == 2020, "SELFTEST FAIL touch calibration values");
    ensure!(
        hardware_button_from_code(193) == HardwareButton::PreviousPage
            && hardware_button_from_code(194) == HardwareButton::NextPage
            && hardware_button_from_code(116) == HardwareButton::Power,
        "SELFTEST FAIL hardware button mapping"
    );
    println!("SELFTEST PASS input_lifecycle long_press_ms=700 cancellation=true buttons=true calibration=true");

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

    let scheduler_profile = DeviceRefreshProfile {
        cleanup_after_fast_updates: 6,
        cleanup_after_screen_areas: 3,
        idle_cleanup_after: Duration::from_millis(100),
    };
    let scheduler_started = Instant::now();
    let mut scheduler = RepaintScheduler::new(scheduler_profile);
    scheduler.enqueue(
        FrameUpdate {
            image: RgbaImage::from_pixel(20, 20, image::Rgba([255, 255, 255, 255])),
            damage: gpui_kobo::PixelRect { x: 2, y: 3, width: 4, height: 5 },
        },
        true,
    );
    scheduler.enqueue(
        FrameUpdate {
            image: RgbaImage::from_pixel(20, 20, image::Rgba([240, 240, 240, 255])),
            damage: gpui_kobo::PixelRect { x: 5, y: 6, width: 4, height: 5 },
        },
        true,
    );
    let coalesced = scheduler.flush(scheduler_started).context("SELFTEST FAIL repaint flush")?;
    ensure!(
        coalesced.mode == RefreshMode::FastMono
            && coalesced.update.damage.x == 2
            && coalesced.update.damage.y == 3
            && coalesced.update.damage.width == 7
            && coalesced.update.damage.height == 8,
        "SELFTEST FAIL repaint coalescing"
    );
    let idle = scheduler
        .idle_cleanup(scheduler_started + Duration::from_millis(101))
        .context("SELFTEST FAIL idle cleanup was not scheduled")?;
    ensure!(idle.mode == RefreshMode::FullGray, "SELFTEST FAIL idle cleanup waveform");
    ensure!(presenter_self_test(&idle.update.image, idle.update.damage)? == 400, "SELFTEST FAIL native presenter buffer sizing");
    println!("SELFTEST PASS repaint_scheduler coalesced=true idle_cleanup=true direct_buffer_pixels=400");

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
    let coverage = extended_renderer_self_test().context("SELFTEST FAIL extended CPU renderer")?;
    println!(
        "SELFTEST PASS extended_renderer painted_pixels={} rounded_corners={} gradients=flattened paths=true shadows=omitted wavy_underlines=true",
        coverage.painted_pixels, coverage.rounded_corner_pixels
    );

    let production_started = Instant::now();
    let initial_run = run_platform_application(&Options {
        output: PathBuf::from(DEFAULT_OUTPUT),
        display: false,
        interactive: false,
        self_test: false,
        timeout: Duration::from_secs(1),
    }, Vec::new()).context("SELFTEST FAIL production Kobo platform")?;
    let initial = initial_run.image;
    let initial_render_ms = production_started.elapsed().as_millis();
    let antialiased_pixels = initial
        .pixels()
        .filter(|pixel| !matches!(pixel.0[0], 24 | 224 | 240 | 255))
        .count();
    ensure!(
        antialiased_pixels > 100,
        "SELFTEST FAIL text rasterization produced only {antialiased_pixels} antialiased pixels"
    );
    println!(
        "SELFTEST PASS text_rasterization antialiased_pixels={antialiased_pixels} production_render_ms={initial_render_ms}"
    );

    let scripted_options = Options {
        output: PathBuf::from(DEFAULT_OUTPUT),
        display: false,
        interactive: false,
        self_test: false,
        timeout: Duration::from_secs(1),
    };
    let swipe_started = Instant::now();
    let swipe = run_platform_application(&scripted_options, vec![
        RuntimeEvent::Touch { mapped: MappedTouch { phase: TouchPhase::Down, x: 300.0, y: 500.0 }, gesture: None },
        RuntimeEvent::Touch { mapped: MappedTouch { phase: TouchPhase::Move, x: 300.0, y: 420.0 }, gesture: None },
        RuntimeEvent::Touch { mapped: MappedTouch { phase: TouchPhase::Move, x: 300.0, y: 340.0 }, gesture: None },
        RuntimeEvent::Touch { mapped: MappedTouch { phase: TouchPhase::Up, x: 300.0, y: 300.0 }, gesture: Some(Gesture::Swipe { dx: 0.0, dy: -200.0 }) },
    ]).context("SELFTEST FAIL production swipe path")?;
    ensure!(swipe.render_count == 2, "SELFTEST FAIL production swipe rendered {} frames instead of initial+release", swipe.render_count);
    ensure!(changed_pixels(&initial, &swipe.image) > 100, "SELFTEST FAIL production swipe did not change the framebuffer");
    println!("SELFTEST PASS repaint_guard renders_during_drag=0 release_renders=1 release_ms={}", swipe_started.elapsed().as_millis());

    let page = run_platform_application(&scripted_options, vec![RuntimeEvent::Button(gpui_kobo::ButtonEvent {
        button: HardwareButton::NextPage,
        pressed: true,
        repeated: false,
    })]).context("SELFTEST FAIL production page-button path")?;
    ensure!(page.render_count == 2, "SELFTEST FAIL page button render count {}", page.render_count);
    ensure!(changed_pixels(&initial, &page.image) > 100, "SELFTEST FAIL page button did not change the framebuffer");

    let exit = run_platform_application(&scripted_options, vec![
        RuntimeEvent::Touch { mapped: MappedTouch { phase: TouchPhase::Down, x: 510.0, y: 54.0 }, gesture: None },
        RuntimeEvent::Touch { mapped: MappedTouch { phase: TouchPhase::Up, x: 510.0, y: 54.0 }, gesture: Some(Gesture::Tap { x: 510.0, y: 54.0 }) },
    ]).context("SELFTEST FAIL production exit path")?;
    ensure!(exit.quit_requested, "SELFTEST FAIL EXIT did not quit the production platform");
    ensure!(exit.render_count == 1, "SELFTEST FAIL EXIT caused an unnecessary render");
    println!("SELFTEST PASS production_input page_button=true exit=true exit_repaints=0");

    ensure!(
        initial.dimensions() == (600, 800),
        "SELFTEST FAIL production platform framebuffer dimensions"
    );
    println!(
        "SELFTEST PASS production_platform frame={}x{} elapsed_ms={}",
        initial.width(),
        initial.height(),
        initial_render_ms
    );
    println!("SELFTEST PASS all total_ms={}", started.elapsed().as_millis());
    Ok(())
}

fn changed_pixels(before: &RgbaImage, after: &RgbaImage) -> usize {
    before.pixels().zip(after.pixels()).filter(|(left, right)| left != right).count()
}

fn parse_options() -> Result<Option<Options>> {
    let mut output = PathBuf::from(DEFAULT_OUTPUT);
    let mut display = true;
    let mut interactive = false;
    let mut self_test = false;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECONDS);
    let mut args = env::args_os().skip(1);

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--output") => output = required_value("--output", args.next())?.into(),
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
        display,
        interactive,
        self_test,
        timeout,
    }))
}

fn required_value(name: &str, value: Option<OsString>) -> Result<OsString> {
    value.ok_or_else(|| anyhow::anyhow!("{name} requires a value"))
}

fn print_help() {
    println!(
        "gpui-kobo-button [--output PATH] [--no-display]\n\
         \t[--interactive] [--self-test] [--timeout-seconds N]\n\
         \n\
         Render a text library through GPUI's CPU Kobo renderer. Interactive mode\n\
         uses a persistent linked FBInk session and a Kobo-native input loop.\n\
         Tap EXIT to return. The recovery timeout is\n\
         {DEFAULT_TIMEOUT_SECONDS} seconds."
    );
}
