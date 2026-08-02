use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, bail};
use gpui_kobo::{render_button_test_image, write_pgm};

const DEFAULT_OUTPUT: &str = "/tmp/gpui-kobo-button.pgm";

struct Options {
    output: PathBuf,
    fbink: PathBuf,
    display: bool,
}

fn main() -> Result<()> {
    let Some(options) = parse_options()? else {
        return Ok(());
    };

    let image = render_button_test_image().context("GPUI failed to render the button scene")?;
    write_pgm(&image, &options.output).with_context(|| {
        format!("failed to write {}", options.output.display())
    })?;

    if options.display {
        present_with_fbink(&options.fbink, &options.output)?;
    }

    println!(
        "rendered {}x{} GPUI button to {}",
        image.width(),
        image.height(),
        options.output.display()
    );
    Ok(())
}

fn parse_options() -> Result<Option<Options>> {
    let mut output = PathBuf::from(DEFAULT_OUTPUT);
    let mut fbink = default_fbink_path();
    let mut display = true;
    let mut args = env::args_os().skip(1);

    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--output") => output = required_value("--output", args.next())?.into(),
            Some("--fbink") => fbink = required_value("--fbink", args.next())?.into(),
            Some("--no-display") => display = false,
            Some("-h" | "--help") => {
                print_help();
                return Ok(None);
            }
            _ => bail!("unknown argument: {}", arg.to_string_lossy()),
        }
    }

    Ok(Some(Options { output, fbink, display }))
}

fn required_value(name: &str, value: Option<OsString>) -> Result<OsString> {
    value.ok_or_else(|| anyhow::anyhow!("{name} requires a path"))
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

fn present_with_fbink(fbink: &Path, image: &Path) -> Result<()> {
    let status = Command::new(fbink)
        .args(["-q", "-c", "-f", "-w", "-W", "GC16"])
        .arg("-i")
        .arg(image)
        .args(["-g", "halign=CENTER,valign=CENTER,w=-1,h=-1"])
        .status()
        .with_context(|| format!("failed to start {}", fbink.display()))?;

    if !status.success() {
        bail!("FBInk exited with {status}");
    }
    Ok(())
}

fn print_help() {
    println!(
        "gpui-kobo-button [--output PATH] [--fbink PATH] [--no-display]\n\
         \n\
         Render one button through GPUI's CPU Kobo renderer. By default the\n\
         image is written to {DEFAULT_OUTPUT} and displayed with a sibling\n\
         fbink binary or the fbink found on PATH."
    );
}
