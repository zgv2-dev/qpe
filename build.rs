use flate2::{Compression, write::DeflateEncoder};
use std::hash::Hasher;
use std::io::Write;
use std::path::{Path, PathBuf};

// libmpv2-sys links `mpv` without a search path. Point the linker at it.
fn main() {
    println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo::rustc-check-cfg=cfg(bundled)");
    let os = std::env::var("CARGO_CFG_TARGET_OS");
    if let Ok(dir) = std::env::var("MPV_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    } else if os.as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
        println!("cargo:rustc-link-search=native=/usr/local/lib");
    }
    if os.as_deref() == Ok("windows") {
        // libmpv from MPV_LIB_DIR (needed to link anyway), ffmpeg from PATH.
        let mpv = std::env::var_os("MPV_LIB_DIR").map(|d| Path::new(&d).join("libmpv-2.dll"));
        let ffmpeg = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("ffmpeg.exe"))
                .find(|f| f.is_file())
        });
        match (mpv.filter(|f| f.is_file()), ffmpeg) {
            (Some(mpv), Some(ffmpeg)) => bundle([("libmpv-2.dll", mpv), ("ffmpeg.exe", ffmpeg)]),
            _ => println!(
                "cargo:warning=libmpv-2.dll (MPV_LIB_DIR) or ffmpeg.exe (PATH) not found; qpe.exe will need them next to it"
            ),
        }
    }
}

/// Single-file Windows build: deflate libmpv and ffmpeg for `src/bundle.rs` to embed.
fn bundle(files: [(&str, PathBuf); 2]) {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut id = std::hash::DefaultHasher::new();
    for (name, src) in files {
        println!("cargo:rerun-if-changed={}", src.display());
        let data = std::fs::read(&src).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
        id.write(&data);
        let dst = std::fs::File::create(out.join(format!("{name}.deflate"))).unwrap();
        let mut enc = DeflateEncoder::new(dst, Compression::default());
        enc.write_all(&data).unwrap();
        enc.finish().unwrap();
    }
    println!("cargo:rustc-env=QPE_BUNDLE_ID={:016x}", id.finish());
    println!("cargo:rustc-cfg=bundled");
    // libmpv loads on first use, after `bundle::unpack` has put it on disk.
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:libmpv-2.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
