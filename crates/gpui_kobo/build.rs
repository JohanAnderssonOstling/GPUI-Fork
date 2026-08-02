use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=FBINK_LIB_DIR");
    let target = env::var("TARGET").unwrap_or_default();
    if !target.starts_with("armv7-") {
        return;
    }

    let library_dir = env::var("FBINK_LIB_DIR")
        .expect("FBINK_LIB_DIR must point to the staged static FBInk libraries for Kobo builds");
    println!("cargo:rustc-link-search=native={library_dir}");
    let i2c_dir = Path::new(&library_dir)
        .parent()
        .expect("FBINK_LIB_DIR must have an FBInk source parent")
        .join("libi2c-staged/lib");
    println!("cargo:rustc-link-search=native={}", i2c_dir.display());
    println!("cargo:rustc-link-lib=static=gpui_fbink_shim");
    println!("cargo:rustc-link-lib=static=fbink");
    println!("cargo:rustc-link-lib=static=i2c");
    println!("cargo:rustc-link-lib=m");
}
