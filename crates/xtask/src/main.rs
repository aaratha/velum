// crates/xtask/src/main.rs
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, env, fs, path::{Path, PathBuf}};

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    checksums: HashMap<String, String>,
}

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("fetch-pdfium") => fetch_pdfium(),
        _ => {
            eprintln!("usage: cargo xtask fetch-pdfium");
            std::process::exit(1);
        }
    }
}

fn target_key() -> Result<&'static str> {
    Ok(match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("windows", "x86_64") => "win-x64",
        ("linux", "x86_64") => "linux-x64",
        (os, arch) => bail!("unsupported target: {os}-{arch}"),
    })
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fetch_pdfium() -> Result<()> {
    let root = workspace_root();
    let manifest: Manifest = toml::from_str(&fs::read_to_string(root.join("pdfium.toml"))?)?;
    let key = target_key()?;
    let expected = manifest.checksums.get(key)
        .with_context(|| format!("no checksum pinned for {key} in pdfium.toml"))?;

    let dest = root.join(".pdfium-cache").join(&manifest.version).join(key);
    if dest.join(".complete").exists() {
        println!("pdfium already cached at {}", dest.display());
        return Ok(());
    }
    fs::create_dir_all(&dest)?;

    let url = format!(
        "https://github.com/bblanchon/pdfium-binaries/releases/download/{}/pdfium-{key}.tgz",
        manifest.version.replace('/', "%2F"),
    );
    println!("downloading {url}");
    let mut bytes = Vec::new();
    ureq::get(&url).call()?.into_reader().read_to_end(&mut bytes)?;

    let actual = format!("{:x}", Sha256::digest(&bytes));
    if &actual != expected {
        bail!("checksum mismatch for {key}\n  expected: {expected}\n  actual:   {actual}");
    }

    tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice())).unpack(&dest)?;
    fs::write(dest.join(".complete"), "")?;
    println!("pdfium {} ({key}) ready at {}", manifest.version, dest.display());
    Ok(())
}
