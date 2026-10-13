//! libmpv playback, rendered into an OpenGL framebuffer and blitted into the egui frame.

use eframe::egui::{self, Rect};
use eframe::egui_glow;
use eframe::glow::{self, HasContext as _};
use libmpv2::events::{Event, PropertyData};
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use libmpv2::{Format, Mpv};
use std::ffi::{CStr, CString, c_void};
use std::sync::Arc;
use std::time::{Duration, Instant};

type GetProc = Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;

fn get_proc(gp: &GetProc, name: &str) -> *mut c_void {
    CString::new(name).map_or(std::ptr::null_mut(), |s| gp(&s).cast_mut())
}

/// Properties mpv pushes to us. Reading them with `get_property` each frame
/// blocks the UI whenever mpv's core is busy (decoding a seek), so we observe them instead.
const OBSERVED: &[(&str, Format)] = &[
    ("time-pos", Format::Double),
    ("pause", Format::Flag),
    ("width", Format::Int64),
    ("height", Format::Int64),
    ("video-params/aspect", Format::Double),
    ("video-params/rotate", Format::Int64),
];

pub struct Props {
    pub pos: f64,
    pub paused: bool,
    pub width: i64,
    pub height: i64,
    pub aspect: Option<f64>,
    pub rotate: i64,
}

pub struct Player {
    // Field order matters: the render context must be dropped before mpv.
    render: RenderContext<'static>,
    pub mpv: Mpv,
    gl: Arc<glow::Context>,
    fbo: Option<(glow::Framebuffer, glow::Texture, i32, i32)>,
    pub props: Props,
    /// When the seek mpv is working on started; further seeks wait for it.
    seeking: Option<Instant>,
    /// Latest seek target that arrived while mpv was busy.
    queued: Option<f64>,
}

impl Player {
    pub fn new(cc: &eframe::CreationContext) -> Result<Self, String> {
        let gl = cc.gl.clone().ok_or("OpenGL is unavailable")?;
        let gp = cc.get_proc_address.clone().ok_or("OpenGL is unavailable")?;
        let mut mpv = Mpv::with_initializer(|init| {
            init.set_option("vo", "libmpv")?;
            init.set_option("hwdec", "auto-safe")?;
            init.set_option("keep-open", "yes")?;
            init.set_option("hr-seek", "yes")?;
            // Otherwise each render call sleeps until the frame's display time, stalling the UI.
            init.set_option("video-timing-offset", "0")?;
            init.set_option("pause", true)
        })
        .map_err(|e| e.to_string())?;
        let render = mpv
            .create_render_context([
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address: get_proc,
                    ctx: gp,
                }),
            ])
            .map_err(|e| e.to_string())?;
        // SAFETY: erases the borrow of `mpv` only; the context holds the raw handle and is dropped first.
        let mut render: RenderContext<'static> = unsafe { std::mem::transmute(render) };
        let ctx = cc.egui_ctx.clone();
        render.set_update_callback(move || ctx.request_repaint());
        let ctx = cc.egui_ctx.clone();
        mpv.set_wakeup_callback(move || ctx.request_repaint());
        for (i, (name, format)) in OBSERVED.iter().enumerate() {
            mpv.observe_property(name, *format, i as u64)
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            render,
            mpv,
            gl,
            fbo: None,
            props: Props {
                pos: 0.0,
                paused: true,
                width: 16,
                height: 9,
                aspect: None,
                rotate: 0,
            },
            seeking: None,
            queued: None,
        })
    }

    /// Drain mpv's events: refresh cached properties and run the queued seek.
    /// Returns whether a file finished loading, and the last error.
    pub fn poll(&mut self) -> (bool, Option<String>) {
        let (mut loaded, mut failed, mut idle) = (false, None, false);
        let p = &mut self.props;
        while let Some(ev) = self.mpv.wait_event(0.0) {
            match ev {
                Ok(Event::PropertyChange { name, change, .. }) => match (name, change) {
                    ("time-pos", PropertyData::Double(v)) if self.seeking.is_none() => p.pos = v,
                    ("pause", PropertyData::Flag(v)) => p.paused = v,
                    ("width", PropertyData::Int64(v)) => p.width = v,
                    ("height", PropertyData::Int64(v)) => p.height = v,
                    ("video-params/aspect", PropertyData::Double(v)) => p.aspect = Some(v),
                    ("video-params/rotate", PropertyData::Int64(v)) => p.rotate = v,
                    _ => {}
                },
                Ok(Event::FileLoaded) => (loaded, idle) = (true, true),
                Ok(Event::PlaybackRestart | Event::EndFile(_)) => idle = true,
                Ok(_) => {}
                Err(e) => failed = Some(e.to_string()),
            }
        }
        if idle {
            self.seeking = None;
            if let Some(t) = self.queued.take() {
                self.seek(t);
            }
        }
        (loaded, failed)
    }

    pub fn get<T: libmpv2::GetData>(&self, prop: &str) -> Option<T> {
        self.mpv.get_property(prop).ok()
    }

    pub fn cmd(&self, name: &str, args: &[&str]) {
        let _ = self.mpv.command(name, args);
    }

    /// Exact seek. While one is in flight only the latest target is kept, so
    /// scrubbing never piles up seeks or blocks on mpv's busy core.
    pub fn seek(&mut self, t: f64) {
        self.props.pos = t;
        // shortcut: assume a seek that never reports back is lost after 1s; fine for local files.
        if self
            .seeking
            .is_some_and(|s| s.elapsed() < Duration::from_secs(1))
        {
            self.queued = Some(t);
            return;
        }
        self.queued = None;
        self.seeking = Some(Instant::now());
        self.cmd("seek", &[&format!("{t:.3}"), "absolute+exact"]);
    }

    /// Draw the current video frame into `rect`.
    pub fn paint(&mut self, ui: &egui::Ui, rect: Rect) {
        let ppp = ui.ctx().pixels_per_point();
        let (w, h) = ((rect.width() * ppp) as i32, (rect.height() * ppp) as i32);
        if w <= 0 || h <= 0 {
            return;
        }
        let gl = &self.gl;
        let resized = self.fbo.is_none_or(|f| (f.2, f.3) != (w, h));
        // SAFETY: eframe calls `ui` on the thread that owns the GL context.
        unsafe {
            if resized {
                if let Some((fb, tex, ..)) = self.fbo.take() {
                    gl.delete_framebuffer(fb);
                    gl.delete_texture(tex);
                }
                let (Ok(tex), Ok(fb)) = (gl.create_texture(), gl.create_framebuffer()) else {
                    return;
                };
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    w,
                    h,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fb));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(tex),
                    0,
                );
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                self.fbo = Some((fb, tex, w, h));
            }
        }
        let Some((fb, ..)) = self.fbo else { return };
        // Render on every UI pass: gating on mpv's update flag misses the frame after a paused seek.
        let _ = self.render.update();
        let _ = self.render.render::<GetProc>(fb.0.get() as i32, w, h, true);
        let cb = egui_glow::CallbackFn::new(move |info, painter| unsafe {
            let gl = painter.gl();
            let prev = gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING) as u32;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fb));
            let s = info.pixels_per_point;
            let sh = info.screen_size_px[1] as f32;
            gl.blit_framebuffer(
                0,
                0,
                w,
                h,
                (rect.min.x * s) as i32,
                (sh - rect.max.y * s) as i32,
                (rect.max.x * s) as i32,
                (sh - rect.min.y * s) as i32,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );
            gl.bind_framebuffer(
                glow::READ_FRAMEBUFFER,
                std::num::NonZeroU32::new(prev).map(glow::NativeFramebuffer),
            );
        });
        ui.painter().add(egui::PaintCallback {
            rect,
            callback: Arc::new(cb),
        });
    }
}
