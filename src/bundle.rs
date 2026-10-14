//! Single-file Windows build: libmpv and ffmpeg ride inside qpe.exe, deflated,
//! and are unpacked to `%LOCALAPPDATA%\QPE\<id>` on first run. See `build.rs`.

use flate2::read::DeflateDecoder;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

/// Hash of the embedded files, so each build unpacks to its own folder.
const ID: &str = env!("QPE_BUNDLE_ID");
const FILES: [(&str, &[u8]); 2] = [
    (
        "libmpv-2.dll",
        include_bytes!(concat!(env!("OUT_DIR"), "/libmpv-2.dll.deflate")),
    ),
    (
        "ffmpeg.exe",
        include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.exe.deflate")),
    ),
];

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut std::ffi::c_void;
}

pub fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("QPE")
        .join(ID)
}

/// Unpack missing files, delete other builds' copies, and load libmpv so its delay-loaded imports resolve.
pub fn unpack() -> std::io::Result<()> {
    let dir = dir();
    fs::create_dir_all(&dir)?;
    for (name, data) in FILES {
        let path = dir.join(name);
        if path.is_file() {
            continue;
        }
        // Write aside and rename, so a crash never leaves a truncated file behind the real name.
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        std::io::copy(&mut DeflateDecoder::new(data), &mut fs::File::create(&tmp)?)?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            // Another QPE launched at the same time got there first.
            if !path.is_file() {
                return Err(e);
            }
        }
    }
    // A build that is still running keeps its files locked; it gets cleaned up next time.
    if let Some(Ok(builds)) = dir.parent().map(fs::read_dir) {
        for b in builds.flatten().filter(|b| b.file_name() != ID) {
            let _ = fs::remove_dir_all(b.path());
        }
    }
    let dll: Vec<u16> = dir
        .join(FILES[0].0)
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect();
    // SAFETY: `dll` is a NUL-terminated UTF-16 path.
    if unsafe { LoadLibraryW(dll.as_ptr()) }.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
