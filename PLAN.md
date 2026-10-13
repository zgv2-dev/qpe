# QPE — Quick Look–style video trim & crop

Goal: open a video, trim and/or crop it in a few seconds, save. It should feel as easy as Quick Look / QuickTime trim.
Platforms: macOS, Windows, Linux. Phase 1 formats: MP4 and MKV.

## Stack

- **Rust nightly**, pinned with `rust-toolchain.toml`.
- **egui / eframe** with the `glow` (OpenGL) backend for the UI. Immediate mode makes the custom trim bar and crop overlay small. MIT/Apache licensed. eframe exposes the GL context and `get_proc_address`, which mpv needs.
- **libmpv** (crate `libmpv2`) for playback. Plays MP4, MKV and nearly everything else with hardware decoding on all three OSes. No proxy files. Exact seeking (`seek <t> absolute+exact`) and frame stepping are built in.
- **ffmpeg CLI** (run with `std::process::Command`) for filmstrip thumbnails and all exports.
- `rfd` for native open/save dialogs, `egui-phosphor` for icons.

Why egui over Slint: custom widgets are simpler in immediate mode, and the license is permissive. Slint stays an option if the UI needs more polish later.

## Video rendering

1. Create the mpv render context with eframe's `get_proc_address` (OpenGL API).
2. Each frame, if mpv signals an update, render into an FBO-backed GL texture sized to the video panel.
3. Show that texture in egui as a native texture (`egui_glow::Painter::register_native_texture`).
4. mpv's update callback calls `ctx.request_repaint()`, so the UI repaints only when needed.

Risk: this is the plumbing-heavy part. It is milestone 1 so it is proven before anything else. macOS OpenGL is deprecated but still works (GL 4.1 core).

## Export rules

- Trim only → `ffmpeg -ss IN -to OUT -i SRC -map 0 -c copy -avoid_negative_ts make_zero OUT`. Lossless, near-instant. The start may snap to the previous keyframe.
- Crop, or "Precise trim" toggle → `-vf crop=W:H:X:Y`, re-encode video, copy audio.
- Encoder pick: at startup, parse `ffmpeg -encoders` and use the first available of: VideoToolbox (macOS), NVENC / QSV / AMF (Windows), VAAPI / NVENC (Linux), then software (`libx264`, else `libopenh264`). Keep the source codec family (H.264 or HEVC) when possible.
- Output container = input container. Default name `<name>-edited.<ext>` next to the source. Never overwrite the source silently.
- Progress via `-progress pipe:1` (`out_time_us`) on a worker thread, sent to the UI over a channel. Cancel = kill the process and delete the partial file.

## UX

- Open: Ctrl/⌘+O, drag-drop onto the window, file path as a CLI argument (enables "Open With" and file associations).
- Space = play/pause. ←/→ = frame step. I / O = set in / out.
- Trim bar: filmstrip (about 20 thumbnails from ffmpeg, made in the background), yellow handles, playhead.
- Crop: dim outside the rect, 8 handles, presets Free, Original, 16:9, 9:16, 1:1, 4:3. Map panel coords to source pixels (rotation and display aspect from mpv). Round W/H/X/Y to even numbers (yuv420 requirement).

## Layout

```
qpe/
  Cargo.toml, build.rs      build.rs points the linker at libmpv (MPV_LIB_DIR, or Homebrew on macOS)
  rust-toolchain.toml       nightly
  src/
    main.rs       eframe setup, file from argv
    app.rs        state, shortcuts, video stage + crop overlay, bottom bar, trim bar
    player.rs     libmpv + GL render context, FBO blit
    crop.rs       crop math (pure, unit tested)
    ffmpeg.rs     export arg builder, encoder fallback, runner, thumbnails (+ export round-trip test)
  assets/         icon, Linux .desktop
  scripts/        make-samples.sh, package-macos.sh
  .github/workflows/ci.yml   test + package on all three OSes, release on v* tags
```

No ffprobe step: mpv already reports duration, size, rotation and codec.

## Milestones

0. **Setup.** Cargo project builds and links libmpv on macOS.
1. **Playback.** Open a file, see video in egui, play/pause, seek. MP4 and MKV both play.
2. **Trim.** Trim bar + I/O keys. Export via stream copy. Test: output duration ≈ selection (±1 GOP).
3. **Crop.** Overlay + presets. Export re-encode. Test: output dimensions match the rect exactly.
4. **Polish.** Progress + cancel, error dialogs with ffmpeg stderr tail, Precise trim toggle, filmstrip.
5. **Windows + Linux.** GitHub Actions matrix builds and runs tests on all three OSes. Verify playback and encoder pick on real machines.
6. **Packaging.** `cargo-packager`: `.app`/DMG, MSI/NSIS, AppImage/deb. Bundle libmpv and ffmpeg per platform. File associations for mp4/mkv.

Later: rotate/flip, MOV/WebM/AVI input, smart-cut (re-encode only the first GOP for frame-exact lossless trims).

## Distribution notes

- libmpv: macOS bundle the dylib and its deps (`dylibbundler`). Windows use the shinchiro `libmpv-2.dll` builds. Linux depend on the distro's `libmpv`, or bundle it in the AppImage.
- ffmpeg: release packages ship GPL builds (Homebrew on macOS, BtbN on Windows/Linux) so `libx264` is there as the software fallback. ffmpeg runs as a separate process, so QPE's own code is not GPL-bound, but the ffmpeg source must be offered alongside.
- libmpv is LGPL when built without GPL options. Dynamic linking is fine.
