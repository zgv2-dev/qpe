//! Everything that shells out to the ffmpeg CLI: export and filmstrip thumbnails.

use eframe::egui::{ColorImage, Context};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};

/// Encoders to try, best first. Unavailable ones are filtered out; failing ones fall through to the next.
const H264: &[&str] = &[
    "h264_videotoolbox",
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "libx264",
    "h264_mf",
    "libopenh264",
];
const HEVC: &[&str] = &[
    "hevc_videotoolbox",
    "hevc_nvenc",
    "hevc_qsv",
    "hevc_amf",
    "libx265",
    "hevc_mf",
];

/// `ffmpeg` bundled with QPE, else the one on PATH.
fn command(name: &str) -> Command {
    #[cfg(bundled)]
    let dir = Some(crate::bundle::dir());
    #[cfg(not(bundled))]
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let bundled = dir
        .map(|d| d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|p| p.is_file());
    let mut cmd = Command::new(bundled.as_deref().unwrap_or(Path::new(name)));
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

pub struct Job {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub start: f64,
    pub end: f64,
    /// `[w, h, x, y]` in source pixels.
    pub crop: Option<[u32; 4]>,
    /// Re-encode even without a crop, so the cut lands on the exact frame.
    pub precise: bool,
    /// Source video codec as named by ffmpeg (`h264`, `hevc`, ...).
    pub codec: String,
    /// Index into [`QUALITIES`]. Anything below the best re-encodes.
    pub quality: usize,
    /// Drop all audio tracks.
    pub mute: bool,
}

/// Export containers, by file extension.
pub const FORMATS: &[&str] = &["mp4", "mov", "mkv"];
/// Quality levels, best first. Only the best one may stream-copy.
pub const QUALITIES: &[&str] = &["High", "Medium", "Low"];

pub enum Msg {
    Progress(f32),
    Done(Result<(), String>),
}

pub struct Running {
    pub rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

impl Running {
    /// Stops ffmpeg and deletes the partial output.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Encoders worth trying for a source codec, given the output of `ffmpeg -encoders`.
pub fn candidates(codec: &str, available: &str) -> Vec<&'static str> {
    let has = |e: &&str| {
        available
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(*e))
    };
    let hevc = if codec == "hevc" { HEVC } else { &[] };
    hevc.iter()
        .chain(H264)
        .copied()
        .filter(|e| has(e))
        .collect()
}

/// Encoder settings for `level`, an index into [`QUALITIES`].
fn quality(enc: &str, level: usize) -> Vec<String> {
    let q = |v: [u32; 3]| v[level.min(2)];
    match enc.rsplit('_').next().unwrap_or(enc) {
        "videotoolbox" => format!("-q:v {}", q([65, 50, 35])),
        "nvenc" => format!("-rc vbr -cq {} -b:v 0", q([23, 28, 33])),
        "qsv" => format!("-global_quality {}", q([23, 28, 33])),
        "amf" => format!("-rc cqp -qp_i {0} -qp_p {0}", q([22, 28, 34])),
        "libx264" => format!("-crf {} -preset medium", q([20, 24, 28])),
        "libx265" => format!("-crf {} -preset medium", q([22, 26, 30])),
        // shortcut: fixed bitrate for encoders without a quality mode, revisit if output looks soft.
        _ => format!("-b:v {}M", q([12, 6, 3])),
    }
    .split(' ')
    .map(String::from)
    .collect()
}

/// ffmpeg arguments for `job`. `enc = None` means lossless stream copy.
pub fn args(job: &Job, enc: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        "-hide_banner",
        "-nostdin",
        "-y",
        "-v",
        "error",
        "-progress",
        "pipe:1",
        "-nostats",
    ]
    .map(String::from)
    .into();
    a.extend([
        "-ss".into(),
        format!("{:.3}", job.start),
        "-to".into(),
        format!("{:.3}", job.end),
    ]);
    a.extend(["-i".into(), job.src.to_string_lossy().into_owned()]);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match enc {
        None => a.extend(s(&[
            "-map",
            "0",
            "-dn",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
        ])),
        Some(enc) => {
            a.extend(s(&[
                "-map", "0:v:0", "-map", "0:a?", "-map", "0:s?", "-c", "copy", "-c:v", enc,
            ]));
            a.extend(quality(enc, job.quality));
            if let Some([w, h, x, y]) = job.crop {
                a.extend(["-vf".into(), format!("crop={w}:{h}:{x}:{y}")]);
            }
        }
    }
    if job.mute {
        a.push("-an".into());
    }
    let mkv = |p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mkv"));
    // Subtitles and attachments don't carry across containers: MP4/MOV only take mov_text, MKV can't.
    match (mkv(&job.src), mkv(&job.dst)) {
        (true, false) => a.extend(s(&["-map", "-0:t", "-c:s", "mov_text"])),
        (false, true) => a.extend(s(&["-c:s", "srt"])),
        _ => {}
    }
    if enc.map_or(job.codec == "hevc", |e| HEVC.contains(&e)) && !mkv(&job.dst) {
        a.extend(s(&["-tag:v", "hvc1"])); // so QuickTime/Apple players open HEVC MP4s
    }
    a.push(job.dst.to_string_lossy().into_owned());
    a
}

pub fn export(job: Job, ctx: Context) -> Running {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    std::thread::spawn(move || {
        let res = run(&job, &tx, &stop, &ctx);
        if stop.load(Ordering::Relaxed) || res.is_err() {
            let _ = std::fs::remove_file(&job.dst);
        }
        if !stop.load(Ordering::Relaxed) {
            let _ = tx.send(Msg::Done(res));
            ctx.request_repaint();
        }
    });
    Running { rx, cancel }
}

fn run(job: &Job, tx: &Sender<Msg>, stop: &AtomicBool, ctx: &Context) -> Result<(), String> {
    static ENCODERS: OnceLock<String> = OnceLock::new();
    let copy = job.crop.is_none() && !job.precise && job.quality == 0;
    let list = ENCODERS.get_or_init(|| {
        command("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    });
    // Copy when allowed. Re-encode if it fails, e.g. a codec the new container can't hold.
    let encs: Vec<Option<&str>> = copy
        .then_some(None)
        .into_iter()
        .chain(candidates(&job.codec, list).into_iter().map(Some))
        .collect();
    if encs.is_empty() {
        return Err("ffmpeg has no H.264/HEVC encoder (or ffmpeg was not found).".into());
    }
    let dur = (job.end - job.start).max(0.001);
    let mut err = String::new();
    for enc in encs {
        let mut child = command("ffmpeg")
            .args(args(job, enc))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Can't run ffmpeg: {e}"))?;
        let mut stderr = child.stderr.take().unwrap();
        let errs = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        for line in BufReader::new(child.stdout.take().unwrap())
            .lines()
            .map_while(Result::ok)
        {
            if stop.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }
            if let Some(us) = line
                .strip_prefix("out_time_us=")
                .and_then(|v| v.parse::<f64>().ok())
            {
                let _ = tx.send(Msg::Progress((us / 1e6 / dur).clamp(0.0, 1.0) as f32));
                ctx.request_repaint();
            }
        }
        let ok = child.wait().is_ok_and(|s| s.success());
        if stop.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if ok {
            return Ok(());
        }
        err = errs.join().unwrap_or_default();
    }
    let tail: Vec<&str> = err.lines().rev().take(3).collect();
    Err(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

/// Grab `n` evenly spaced `w`×`h` frames in the background. Stops early once `tx`'s receiver is dropped.
pub fn thumbnails(
    src: PathBuf,
    dur: f64,
    n: usize,
    [w, h]: [usize; 2],
    tx: Sender<(usize, ColorImage)>,
    ctx: Context,
) {
    std::thread::spawn(move || {
        for i in 0..n {
            let t = dur * (i as f64 + 0.5) / n as f64;
            let Ok(out) = command("ffmpeg")
                .args(["-v", "error", "-ss", &format!("{t:.3}"), "-i"])
                .arg(&src)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    &format!("scale={w}:{h}:force_original_aspect_ratio=increase,crop={w}:{h}"),
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgba",
                    "-",
                ])
                .output()
            else {
                return;
            };
            if out.stdout.len() != w * h * 4 {
                continue;
            }
            if tx
                .send((i, ColorImage::from_rgba_unmultiplied([w, h], &out.stdout)))
                .is_err()
            {
                return;
            }
            ctx.request_repaint();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = " V....D h264_videotoolbox    VideoToolbox H.264 Encoder (codec h264)\n V....D libx264              libx264 H.264\n V....D hevc_videotoolbox    VideoToolbox H.265\n";

    fn job(crop: Option<[u32; 4]>, dst: &str) -> Job {
        Job {
            src: "in.mkv".into(),
            dst: dst.into(),
            start: 1.5,
            end: 4.0,
            crop,
            precise: false,
            codec: "hevc".into(),
            quality: 0,
            mute: false,
        }
    }

    #[test]
    fn candidates_prefer_same_codec_then_h264() {
        assert_eq!(
            candidates("hevc", LIST),
            ["hevc_videotoolbox", "h264_videotoolbox", "libx264"]
        );
        assert_eq!(candidates("vp9", LIST), ["h264_videotoolbox", "libx264"]);
        assert!(candidates("h264", "").is_empty());
    }

    #[test]
    fn copy_args() {
        let a = args(&job(None, "out.mkv"), None).join(" ");
        assert!(
            a.contains("-ss 1.500 -to 4.000 -i in.mkv -map 0 -dn -c copy"),
            "{a}"
        );
        assert!(a.ends_with(" out.mkv"));
        let mut j = job(None, "out.mp4");
        j.mute = true;
        let a = args(&j, None).join(" ");
        assert!(
            a.ends_with("-an -map -0:t -c:s mov_text -tag:v hvc1 out.mp4"),
            "{a}"
        );
    }

    #[test]
    fn crop_args() {
        let a = args(
            &job(Some([640, 360, 2, 4]), "out.mp4"),
            Some("hevc_videotoolbox"),
        )
        .join(" ");
        assert!(
            a.contains("-c:v hevc_videotoolbox -q:v 65 -vf crop=640:360:2:4 -map -0:t"),
            "{a}"
        );
        assert!(a.ends_with("-tag:v hvc1 out.mp4"), "{a}");
        let mut j = job(None, "out.mkv");
        j.quality = 2;
        let a = args(&j, Some("libx264")).join(" ");
        assert!(
            a.contains("-c:v libx264 -crf 28 -preset medium out.mkv"),
            "{a}"
        );
        let a = args(
            &job(Some([640, 360, 2, 4]), "out.mkv"),
            Some("hevc_videotoolbox"),
        )
        .join(" ");
        assert!(!a.contains("hvc1"));
    }

    /// Needs `ffmpeg` and `ffprobe` on PATH.
    #[test]
    fn export_round_trip() {
        let dir = std::env::temp_dir().join(format!("qpe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mkv");
        let ok = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=25:duration=4",
            ])
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=duration=4",
                "-c:v",
                "libx264",
                "-g",
                "25",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&src)
            .status()
            .unwrap();
        assert!(ok.success());
        for (crop, name, size, want) in [
            // Stream copy starts at a keyframe before IN, so allow up to one GOP (1 s) extra.
            (None, "copy.mkv", "640,360", 1.9..3.4),
            (Some([320, 180, 10, 20]), "crop.mp4", "320,180", 1.85..2.15),
        ] {
            let dst = dir.join(name);
            let job = Job {
                src: src.clone(),
                dst: dst.clone(),
                start: 1.0,
                end: 3.0,
                crop,
                precise: false,
                codec: "h264".into(),
                quality: 0,
                mute: false,
            };
            let run = export(job, Context::default());
            let res = loop {
                match run
                    .rx
                    .recv_timeout(std::time::Duration::from_secs(120))
                    .expect("export timed out")
                {
                    Msg::Done(r) => break r,
                    Msg::Progress(_) => {}
                }
            };
            res.unwrap_or_else(|e| panic!("{name}: {e}"));
            let probe = |entries: &str| {
                let o = Command::new("ffprobe")
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-show_entries",
                        entries,
                        "-of",
                        "csv=p=0",
                    ])
                    .arg(&dst)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&o.stdout).trim().to_string()
            };
            assert_eq!(probe("stream=width,height"), size, "{name}");
            let dur: f64 = probe("format=duration").parse().unwrap();
            assert!(want.contains(&dur), "{name}: duration {dur}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
