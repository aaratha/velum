// crates/app/build.rs
use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pin = fs::read_to_string(root.join("pdfium.toml")).expect("missing pdfium.toml");
    let version = pin.lines()
        .find(|l| l.starts_with("version"))
        .and_then(|l| l.split('"').nth(1))
        .expect("couldn't parse version from pdfium.toml");

    let key = match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("windows", "x86_64") => "win-x64",
        ("linux", "x86_64") => "linux-x64",
        _ => panic!("unsupported target"),
    };

    let cache_dir = root.join(".pdfium-cache").join(version).join(key);
    if !cache_dir.join(".complete").exists() {
        panic!("pdfium not found for {key}. Run `cargo xtask fetch-pdfium` first.");
    }

    let lib_name = if cfg!(target_os = "windows") { "pdfium.dll" }
        else if cfg!(target_os = "macos") { "libpdfium.dylib" }
        else { "libpdfium.so" };

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let profile_dir = out_dir.ancestors()
        .find(|p| matches!(p.file_name().and_then(|n| n.to_str()), Some("debug" | "release")))
        .expect("couldn't locate target/<profile> from OUT_DIR");

    fs::copy(cache_dir.join("lib").join(lib_name), profile_dir.join(lib_name))
        .expect("failed to stage pdfium library next to binary");

    println!("cargo:rerun-if-changed=../../pdfium.toml");
}
