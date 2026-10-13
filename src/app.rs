use crate::{crop, ffmpeg, player::Player};
use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Key, Layout, Modifiers, Pos2, Rect, Response,
    RichText, Sense, Stroke, TextureHandle, Ui, Vec2, pos2, vec2,
};
use egui_phosphor::regular as icon;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

const YELLOW: Color32 = Color32::from_rgb(255, 196, 0);
/// macOS dark-mode system blue.
const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
/// macOS dark window background.
const BG: Color32 = Color32::from_rgb(30, 30, 32);
const DIM: Color32 = Color32::from_black_alpha(170);
const THUMBS: usize = 12;
/// Filmstrip tile size in pixels. ffmpeg center-crops frames to it.
const THUMB: [usize; 2] = [96, 72];
/// Shortest selectable trim, in seconds.
const MIN_LEN: f64 = 0.1;
/// Trim handle width, in points.
const HANDLE: f32 = 12.0;

#[derive(Clone, Copy)]
enum Drag {
    /// Crop handle, see [`crop::drag`].
    Crop(i8, i8),
    Move,
    In,
    Out,
    Scrub,
}

struct Video {
    path: PathBuf,
    dur: f64,
    /// Frame size after rotation, in pixels. This is what ffmpeg's crop filter sees.
    size: [u32; 2],
    /// Display aspect ratio after rotation.
    aspect: f32,
    codec: String,
}

pub struct App {
    player: Player,
    loading: Option<PathBuf>,
    video: Option<Video>,
    trim: (f64, f64),
    /// Normalized crop rect while crop mode is on.
    crop: Option<Rect>,
    preset: usize,
    precise: bool,
    drag: Option<Drag>,
    /// Offset from the pointer to the grabbed handle, so it doesn't jump to the pointer.
    grab: Vec2,
    thumbs: Vec<Option<TextureHandle>>,
    thumbs_rx: Option<Receiver<(usize, egui::ColorImage)>>,
    job: Option<ffmpeg::Running>,
    progress: f32,
    /// Message and whether it is an error.
    status: Option<(String, bool)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, file: Option<PathBuf>) -> Result<Self, String> {
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        // San Francisco when the OS has it. Read at runtime: Apple's license forbids bundling.
        for (name, family) in [
            ("SFNS", egui::FontFamily::Proportional),
            ("SFNSMono", egui::FontFamily::Monospace),
        ] {
            if let Ok(data) = std::fs::read(format!("/System/Library/Fonts/{name}.ttf")) {
                fonts
                    .font_data
                    .insert(name.into(), egui::FontData::from_owned(data).into());
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, name.into());
            }
        }
        let mono = fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default();
        mono.push("phosphor".into());
        cc.egui_ctx.set_fonts(fonts);
        theme(&cc.egui_ctx);
        let mut app = Self {
            player: Player::new(cc)?,
            loading: None,
            video: None,
            trim: (0.0, 0.0),
            crop: None,
            preset: 0,
            precise: false,
            drag: None,
            grab: Vec2::ZERO,
            thumbs: Vec::new(),
            thumbs_rx: None,
            job: None,
            progress: 0.0,
            status: None,
        };
        if let Some(f) = file {
            app.open(f);
        }
        Ok(app)
    }

    fn open(&mut self, path: PathBuf) {
        self.player.cmd("loadfile", &[&path.to_string_lossy()]);
        self.loading = Some(path);
        self.status = None;
    }

    fn open_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "m4v", "mkv"])
            .pick_file()
        {
            self.open(p);
        }
    }

    /// Frame size and display aspect after rotation.
    fn geometry(&self) -> ([u32; 2], f32) {
        let p = &self.player.props;
        let (w, h) = (p.width.max(1) as u32, p.height.max(1) as u32);
        let aspect = p.aspect.unwrap_or(w as f64 / h as f64) as f32;
        if p.rotate % 180 == 90 {
            ([h, w], 1.0 / aspect)
        } else {
            ([w, h], aspect)
        }
    }

    fn loaded(&mut self, ctx: &egui::Context) {
        let Some(path) = self.loading.take() else {
            return;
        };
        let dur = self.player.get::<f64>("duration").unwrap_or(0.0);
        let (size, aspect) = self.geometry();
        let codec = self
            .player
            .get::<String>("current-tracks/video/codec")
            .unwrap_or_default();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — QPE")));

        let (tx, rx) = channel();
        ffmpeg::thumbnails(path.clone(), dur, THUMBS, THUMB, tx, ctx.clone());
        self.thumbs = vec![None; THUMBS];
        self.thumbs_rx = Some(rx);

        self.video = Some(Video {
            path,
            dur,
            size,
            aspect,
            codec,
        });
        self.trim = (0.0, dur);
        self.crop = None;
    }

    /// Normalized width/height lock for the current crop preset.
    fn lock(&self) -> Option<f32> {
        let aspect = self.video.as_ref()?.aspect;
        crop::PRESETS[self.preset]
            .1
            .map(|r| if r == 0.0 { 1.0 } else { crop::lock(r, aspect) })
    }

    fn pos(&self) -> f64 {
        self.player.props.pos
    }

    fn paused(&self) -> bool {
        self.player.props.paused
    }

    fn toggle_play(&mut self) {
        let paused = self.paused();
        if paused && !(self.trim.0..self.trim.1 - 0.05).contains(&self.pos()) {
            self.player.seek(self.trim.0);
        }
        let _ = self.player.mpv.set_property("pause", !paused);
    }

    fn set_in(&mut self, t: f64) {
        self.trim.0 = t.clamp(0.0, self.trim.1 - MIN_LEN);
        self.player.seek(self.trim.0);
    }

    fn set_out(&mut self, t: f64) {
        let dur = self.video.as_ref().map_or(0.0, |v| v.dur);
        self.trim.1 = t.clamp(self.trim.0 + MIN_LEN, dur);
        self.player.seek(self.trim.1);
    }

    fn export(&mut self, ctx: &egui::Context) {
        let Some(v) = &self.video else { return };
        if self.job.is_some() {
            return;
        }
        let ext = v
            .path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let stem = v.path.file_stem().unwrap_or_default().to_string_lossy();
        let mut dialog = rfd::FileDialog::new()
            .set_file_name(format!("{stem}-edited.{ext}"))
            .add_filter(&ext, &[&ext]);
        if let Some(dir) = v.path.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(mut dst) = dialog.save_file() else {
            return;
        };
        if dst.extension().is_none() {
            dst.set_extension(&ext);
        }
        if std::fs::canonicalize(&dst).ok() == std::fs::canonicalize(&v.path).ok() {
            self.status = Some((
                "Pick another name: QPE won't overwrite the video it is editing.".into(),
                true,
            ));
            return;
        }
        let [fw, fh] = v.size;
        let crop = self
            .crop
            .map(|c| crop::to_pixels(c, fw, fh))
            .filter(|&c| c != [fw, fh, 0, 0]);
        let job = ffmpeg::Job {
            src: v.path.clone(),
            dst,
            start: self.trim.0,
            end: self.trim.1,
            crop,
            precise: self.precise,
            codec: v.codec.clone(),
        };
        self.job = Some(ffmpeg::export(job, ctx.clone()));
        self.progress = 0.0;
        self.status = None;
        let _ = self.player.mpv.set_property("pause", true);
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let (loaded, failed) = self.player.poll();
        if loaded {
            self.loaded(ctx);
        }
        // Rotation and aspect are only known once the first frame is decoded.
        let geometry = self.video.is_some().then(|| self.geometry());
        if let (Some(v), Some(g)) = (&mut self.video, geometry) {
            (v.size, v.aspect) = g;
        }
        if let Some(e) = failed
            && let Some(p) = self.loading.take()
        {
            self.status = Some((format!("Can't open {}: {e}", p.display()), true));
        }
        if let Some(rx) = &self.thumbs_rx {
            for (i, img) in rx.try_iter() {
                self.thumbs[i] =
                    Some(ctx.load_texture(format!("thumb{i}"), img, Default::default()));
            }
        }
        if let Some(job) = &self.job {
            for msg in job.rx.try_iter() {
                match msg {
                    ffmpeg::Msg::Progress(p) => self.progress = p,
                    ffmpeg::Msg::Done(Ok(())) => {
                        self.status = Some(("Saved".into(), false));
                        self.job = None;
                        break;
                    }
                    ffmpeg::Msg::Done(Err(e)) => {
                        self.status = Some((format!("Export failed: {e}"), true));
                        self.job = None;
                        break;
                    }
                }
            }
        }
        // Stop at the out point, like Quick Look's trim preview.
        if self.video.is_some() && !self.paused() && self.pos() >= self.trim.1 {
            let _ = self.player.mpv.set_property("pause", true);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
        {
            self.open(p);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) {
            self.open_dialog();
        }
        if self.video.is_none() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            self.export(ctx);
        }
        let key = |k| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if key(Key::Space) {
            self.toggle_play();
        }
        if key(Key::ArrowLeft) {
            self.player.cmd("frame-back-step", &[]);
        }
        if key(Key::ArrowRight) {
            self.player.cmd("frame-step", &[]);
        }
        if key(Key::I) {
            self.set_in(self.pos());
        }
        if key(Key::O) {
            self.set_out(self.pos());
        }
        if key(Key::C) {
            self.toggle_crop();
        }
        if key(Key::Escape) {
            self.crop = None;
        }
    }

    fn toggle_crop(&mut self) {
        self.crop = match self.crop {
            Some(_) => None,
            None => Some(self.lock().map_or_else(crop::full, crop::fit)),
        };
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.shortcuts(&ctx);

        if self.video.is_some() {
            egui::Panel::bottom("bar")
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(BG)
                        .inner_margin(egui::Margin::symmetric(12, 10)),
                )
                .show(ui, |ui| self.bar(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(if self.video.is_some() {
                Color32::BLACK
            } else {
                BG
            }))
            .show(ui, |ui| {
                if self.video.is_some() {
                    self.stage(ui);
                } else {
                    self.empty(ui);
                }
                self.status_pill(ui);
            });
    }
}

/// Always dark, like QuickTime. Forcing the theme matters: with the system
/// preference, a light-mode OS gets black text on the black stage.
fn theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    // Dark native title bar too, so it doesn't glow white above the video.
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        let text = Color32::from_gray(236);
        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = Color32::from_rgb(44, 44, 46);
        v.extreme_bg_color = Color32::from_rgb(58, 58, 60);
        v.faint_bg_color = Color32::from_white_alpha(8);
        v.window_stroke = Stroke::new(1.0, Color32::from_white_alpha(28));
        v.window_corner_radius = 12.into();
        v.menu_corner_radius = 10.into();
        let shadow = egui::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(110),
        };
        v.window_shadow = shadow;
        v.popup_shadow = shadow;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        v.hyperlink_color = ACCENT;
        v.override_text_color = None;
        v.weak_text_color = Some(Color32::from_gray(150));
        let w = &mut v.widgets;
        for (wv, fill) in [
            (&mut w.noninteractive, Color32::TRANSPARENT),
            (&mut w.inactive, Color32::from_white_alpha(14)),
            (&mut w.hovered, Color32::from_white_alpha(26)),
            (&mut w.active, Color32::from_white_alpha(38)),
            (&mut w.open, Color32::from_white_alpha(26)),
        ] {
            wv.bg_fill = fill;
            wv.weak_bg_fill = fill;
            wv.bg_stroke = Stroke::NONE;
            wv.fg_stroke = Stroke::new(1.0, text);
            wv.corner_radius = 7.into();
            wv.expansion = 0.0;
        }
        w.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_white_alpha(20));
        w.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_gray(200));
        let sp = &mut s.spacing;
        sp.item_spacing = vec2(8.0, 6.0);
        sp.button_padding = vec2(10.0, 5.0);
        sp.menu_margin = egui::Margin::same(6);
        sp.interact_size.y = 26.0;
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(13.5));
        s.text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(13.5));
    });
}

/// Icon-only button with a tooltip and a screen-reader label.
fn icon_button(ui: &mut Ui, glyph: &str, label: &str, selected: bool) -> Response {
    let r = ui.add(
        egui::Button::selectable(selected, RichText::new(glyph).size(18.0))
            .frame_when_inactive(selected)
            .min_size(vec2(30.0, 30.0)),
    );
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, label));
    r.on_hover_text(label)
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let (h, m, s) = ((t / 3600.0) as u64, (t / 60.0) as u64 % 60, t % 60.0);
    if h > 0 {
        format!("{h}:{m:02}:{s:04.1}")
    } else {
        format!("{m}:{s:04.1}")
    }
}

impl App {
    fn empty(&mut self, ui: &mut Ui) {
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.add_space((ui.available_height() / 2.0 - 70.0).max(0.0));
            if self.loading.is_some() {
                ui.spinner();
                return;
            }
            ui.label(RichText::new(icon::FILE_VIDEO).size(56.0).weak());
            ui.add_space(4.0);
            ui.label(RichText::new("Drop a video here").size(17.0).strong());
            ui.label(RichText::new("MP4 or MKV").weak());
            ui.add_space(12.0);
            let shortcut = ui
                .ctx()
                .format_shortcut(&egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::O));
            if ui
                .add(
                    egui::Button::new(RichText::new("Open…").color(Color32::WHITE))
                        .fill(ACCENT)
                        .corner_radius(8)
                        .min_size(vec2(96.0, 30.0)),
                )
                .on_hover_text(shortcut)
                .clicked()
            {
                self.open_dialog();
            }
        });
    }

    fn status_pill(&mut self, ui: &mut Ui) {
        let Some((msg, err)) = self.status.clone() else {
            return;
        };
        let area = egui::Area::new("status".into()).anchor(Align2::CENTER_TOP, vec2(0.0, 12.0));
        area.show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .corner_radius(20)
                .inner_margin(egui::Margin::symmetric(14, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (glyph, color) = if err {
                            (icon::WARNING, Color32::from_rgb(255, 110, 90))
                        } else {
                            (icon::CHECK, Color32::from_rgb(110, 220, 120))
                        };
                        ui.label(RichText::new(glyph).color(color).size(16.0));
                        ui.label(msg);
                        if icon_button(ui, icon::X, "Dismiss", false).clicked() {
                            self.status = None;
                        }
                    });
                });
        });
    }

    /// The video, fitted to the panel, with the crop overlay on top.
    fn stage(&mut self, ui: &mut Ui) {
        let aspect = self.video.as_ref().map_or(16.0 / 9.0, |v| v.aspect);
        let avail = ui.max_rect().shrink(12.0);
        let size = if avail.width() / avail.height() > aspect {
            vec2(avail.height() * aspect, avail.height())
        } else {
            vec2(avail.width(), avail.width() / aspect)
        };
        let vr = Rect::from_center_size(avail.center(), size);
        self.player.paint(ui, vr);

        let resp = ui.interact(
            vr.expand(20.0),
            ui.id().with("stage"),
            Sense::click_and_drag(),
        );
        let Some(c) = self.crop else {
            if resp.clicked() {
                self.toggle_play();
            }
            return;
        };
        let to_screen = |p: Pos2| vr.min + p.to_vec2() * vr.size();
        let sr = Rect::from_min_max(to_screen(c.min), to_screen(c.max));
        let k = self.lock();

        // What sits under the pointer: a corner, an edge (free aspect only), the inside, or nothing.
        let hit = |p: Pos2| -> Option<Drag> {
            for (hx, hy) in [(-1, -1), (1, -1), (-1, 1), (1, 1)] {
                let corner = pos2(
                    if hx < 0 { sr.min.x } else { sr.max.x },
                    if hy < 0 { sr.min.y } else { sr.max.y },
                );
                if corner.distance(p) < 20.0 {
                    return Some(Drag::Crop(hx, hy));
                }
            }
            if k.is_none() && sr.expand(10.0).contains(p) {
                let near = |a: f32, b: f32| (a - b).abs() < 10.0;
                for (hx, hy, hit) in [
                    (-1, 0, near(p.x, sr.min.x)),
                    (1, 0, near(p.x, sr.max.x)),
                    (0, -1, near(p.y, sr.min.y)),
                    (0, 1, near(p.y, sr.max.y)),
                ] {
                    if hit {
                        return Some(Drag::Crop(hx, hy));
                    }
                }
            }
            sr.contains(p).then_some(Drag::Move)
        };

        // Hit-test where the press began: by the time egui reports a drag the pointer
        // has moved past its threshold, often off a small handle.
        let origin = ui.input(|i| i.pointer.press_origin());
        if resp.drag_started() {
            self.drag = origin.and_then(hit);
            if let (Some(Drag::Crop(hx, hy)), Some(o)) = (self.drag, origin) {
                let pick = |h: i8, lo: f32, hi: f32, p: f32| match h {
                    -1 => lo,
                    1 => hi,
                    _ => p,
                };
                let handle = pos2(
                    pick(hx, sr.min.x, sr.max.x, o.x),
                    pick(hy, sr.min.y, sr.max.y, o.y),
                );
                self.grab = handle - o;
            }
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let np = ((p + self.grab - vr.min) / vr.size()).to_pos2();
            self.crop = Some(match self.drag {
                Some(Drag::Crop(hx, hy)) => crop::drag(c, hx, hy, np, k),
                Some(Drag::Move) => crop::shift(c, resp.drag_delta() / vr.size()),
                _ => c,
            });
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        let hovered = self.drag.or_else(|| resp.hover_pos().and_then(hit));
        if let Some(d) = hovered {
            ui.ctx().set_cursor_icon(match d {
                Drag::Crop(-1, -1) | Drag::Crop(1, 1) => CursorIcon::ResizeNwSe,
                Drag::Crop(-1, 1) | Drag::Crop(1, -1) => CursorIcon::ResizeNeSw,
                Drag::Crop(_, 0) => CursorIcon::ResizeHorizontal,
                Drag::Crop(..) => CursorIcon::ResizeVertical,
                _ if self.drag.is_some() => CursorIcon::Grabbing,
                _ => CursorIcon::Grab,
            });
        }

        let p = ui.painter_at(vr);
        for r in [
            Rect::from_min_max(vr.min, pos2(vr.max.x, sr.min.y)),
            Rect::from_min_max(pos2(vr.min.x, sr.max.y), vr.max),
            Rect::from_min_max(pos2(vr.min.x, sr.min.y), pos2(sr.min.x, sr.max.y)),
            Rect::from_min_max(pos2(sr.max.x, sr.min.y), pos2(vr.max.x, sr.max.y)),
        ] {
            p.rect_filled(r, 0.0, DIM);
        }
        p.rect_stroke(
            sr,
            0.0,
            Stroke::new(1.0, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
        if self.drag.is_some() {
            let thin = Stroke::new(1.0, Color32::from_white_alpha(70));
            for f in [1.0 / 3.0, 2.0 / 3.0] {
                p.vline(sr.lerp_inside(vec2(f, 0.0)).x, sr.y_range(), thin);
                p.hline(sr.x_range(), sr.lerp_inside(vec2(0.0, f)).y, thin);
            }
        }
        let thick = Stroke::new(3.0, Color32::WHITE);
        let arm = 14.0f32.min(sr.width() / 2.0).min(sr.height() / 2.0);
        for (hx, hy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0f32)] {
            let c = pos2(
                if hx < 0.0 { sr.min.x } else { sr.max.x },
                if hy < 0.0 { sr.min.y } else { sr.max.y },
            );
            p.line_segment([c, c - vec2(hx * arm, 0.0)], thick);
            p.line_segment([c, c - vec2(0.0, hy * arm)], thick);
        }
        if k.is_none() {
            let (cx, cy) = (sr.center().x, sr.center().y);
            p.hline(cx - 9.0..=cx + 9.0, sr.min.y, thick);
            p.hline(cx - 9.0..=cx + 9.0, sr.max.y, thick);
            p.vline(sr.min.x, cy - 9.0..=cy + 9.0, thick);
            p.vline(sr.max.x, cy - 9.0..=cy + 9.0, thick);
        }
    }

    fn bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let (glyph, label) = if self.paused() {
                (icon::PLAY, "Play (Space)")
            } else {
                (icon::PAUSE, "Pause (Space)")
            };
            if icon_button(ui, glyph, label, false).clicked() {
                self.toggle_play();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(job) = &self.job {
                    if icon_button(ui, icon::X, "Cancel export", false).clicked() {
                        job.cancel();
                        self.job = None;
                    }
                    ui.add(
                        egui::ProgressBar::new(self.progress)
                            .fill(ACCENT)
                            .corner_radius(4)
                            .desired_width(120.0)
                            .show_percentage(),
                    );
                } else {
                    if icon_button(ui, icon::EXPORT, "Export… (⌘S / Ctrl+S)", false).clicked() {
                        self.export(&ctx);
                    }
                    if icon_button(
                        ui,
                        icon::TARGET,
                        "Frame-exact trim (re-encodes)",
                        self.precise,
                    )
                    .clicked()
                    {
                        self.precise = !self.precise;
                    }
                }
                ui.separator();
                if self.crop.is_some() {
                    let before = self.preset;
                    egui::ComboBox::from_id_salt("aspect")
                        .width(80.0)
                        .selected_text(crop::PRESETS[self.preset].0)
                        .show_index(ui, &mut self.preset, crop::PRESETS.len(), |i| {
                            crop::PRESETS[i].0
                        });
                    if self.preset != before {
                        self.crop = Some(self.lock().map_or_else(crop::full, crop::fit));
                    }
                }
                if icon_button(ui, icon::CROP, "Crop (C)", self.crop.is_some()).clicked() {
                    self.toggle_crop();
                }
                ui.separator();
                ui.label(
                    RichText::new(format!(
                        "{} {}",
                        icon::SCISSORS,
                        fmt_time(self.trim.1 - self.trim.0)
                    ))
                    .monospace(),
                )
                .on_hover_text("Selection length. Set in/out with I and O.");
                ui.label(RichText::new(fmt_time(self.pos())).monospace().strong());
                self.trim_bar(ui);
            });
        });
    }

    /// Filmstrip with Quick Look–style yellow trim handles and a playhead.
    fn trim_bar(&mut self, ui: &mut Ui) {
        let dur = self.video.as_ref().map_or(0.0, |v| v.dur).max(0.001);
        let aspect = THUMB[0] as f32 / THUMB[1] as f32;
        let (r, resp) = ui.allocate_exact_size(
            vec2(ui.available_width().max(60.0), 44.0),
            Sense::click_and_drag(),
        );
        let x = |t: f64| r.min.x + (t / dur) as f32 * r.width();
        let t_at = |px: f32| ((px - r.min.x) / r.width()) as f64 * dur;
        let (xi, xo) = (x(self.trim.0), x(self.trim.1));
        // Handle under `px`, nearest first. Zones are wider than the drawn handles.
        let hit = |px: f32| {
            let (di, dout) = (
                (px - xi - HANDLE / 2.0).abs(),
                (px - xo + HANDLE / 2.0).abs(),
            );
            if di < HANDLE * 1.5 && di <= dout {
                Drag::In
            } else if dout < HANDLE * 1.5 {
                Drag::Out
            } else {
                Drag::Scrub
            }
        };

        if resp.drag_started() || resp.clicked() {
            // Where the press began; see `stage`.
            let px = ui
                .input(|i| i.pointer.press_origin())
                .or(resp.interact_pointer_pos())
                .map_or(0.0, |p| p.x);
            let d = hit(px);
            self.drag = Some(d);
            self.grab = vec2(
                match d {
                    Drag::In => xi - px,
                    Drag::Out => xo - px,
                    _ => 0.0,
                },
                0.0,
            );
            let _ = self.player.mpv.set_property("pause", true);
        }
        if (resp.dragged() || resp.clicked())
            && let Some(p) = resp.interact_pointer_pos()
        {
            let t = t_at(p.x + self.grab.x).clamp(0.0, dur);
            match self.drag {
                Some(Drag::In) => self.set_in(t),
                Some(Drag::Out) => self.set_out(t),
                Some(Drag::Scrub) => self.player.seek(t.clamp(self.trim.0, self.trim.1)),
                _ => {}
            }
        }
        if resp.drag_stopped() || resp.clicked() {
            self.drag = None;
        }
        if matches!(self.drag, Some(Drag::In | Drag::Out))
            || resp
                .hover_pos()
                .is_some_and(|p| !matches!(hit(p.x), Drag::Scrub))
        {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "Trim"));

        let p = ui.painter_at(r);
        p.rect_filled(r, 6.0, Color32::from_gray(40));
        let slot = r.width() / THUMBS as f32;
        for (i, tex) in self.thumbs.iter().enumerate() {
            let Some(tex) = tex else { continue };
            let sr = Rect::from_min_size(
                pos2(r.min.x + i as f32 * slot, r.min.y),
                vec2(slot, r.height()),
            );
            // Center-crop the thumbnail to the slot's aspect.
            let s = (sr.aspect_ratio() / aspect).min(1.0);
            let t = (aspect / sr.aspect_ratio()).min(1.0);
            let uv = Rect::from_center_size(pos2(0.5, 0.5), vec2(s, t));
            p.image(tex.id(), sr, uv, Color32::WHITE);
        }
        p.rect_filled(Rect::from_min_max(r.min, pos2(xi, r.max.y)), 0.0, DIM);
        p.rect_filled(Rect::from_min_max(pos2(xo, r.min.y), r.max), 0.0, DIM);

        let sel = Rect::from_x_y_ranges(xi..=xo, r.y_range());
        p.rect_stroke(sel, 4.0, Stroke::new(3.0, YELLOW), egui::StrokeKind::Inside);
        let font = FontId::proportional(13.0);
        for (hr, glyph) in [
            (
                Rect::from_x_y_ranges(xi..=xi + HANDLE, r.y_range()),
                icon::CARET_LEFT,
            ),
            (
                Rect::from_x_y_ranges(xo - HANDLE..=xo, r.y_range()),
                icon::CARET_RIGHT,
            ),
        ] {
            p.rect_filled(hr, 3.0, YELLOW);
            p.text(
                hr.center(),
                Align2::CENTER_CENTER,
                glyph,
                font.clone(),
                Color32::BLACK,
            );
        }

        let ph = x(self.pos());
        p.vline(ph, r.y_range(), Stroke::new(2.0, Color32::WHITE));
        p.circle_filled(pos2(ph, r.min.y + 2.0), 3.0, Color32::WHITE);
    }
}
