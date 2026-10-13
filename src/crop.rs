//! Crop math on a normalized rect: (0,0) is the video's top-left, (1,1) its bottom-right.

use eframe::egui::{Pos2, Rect, Vec2, pos2};

/// Smallest crop side, as a fraction of the frame.
const MIN: f32 = 0.02;

/// Aspect presets shown in the crop menu. `None` = free, `Some(0.0)` = source aspect.
pub const PRESETS: &[(&str, Option<f32>)] = &[
    ("Free", None),
    ("Original", Some(0.0)),
    ("16:9", Some(16.0 / 9.0)),
    ("9:16", Some(9.0 / 16.0)),
    ("1:1", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
];

pub fn full() -> Rect {
    Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0))
}

/// Normalized width/height ratio that shows as `ratio` on a video with display aspect `video_aspect`.
pub fn lock(ratio: f32, video_aspect: f32) -> f32 {
    ratio / video_aspect
}

/// Largest centered rect with normalized width/height ratio `k`.
pub fn fit(k: f32) -> Rect {
    let (w, h) = if k >= 1.0 { (1.0, 1.0 / k) } else { (k, 1.0) };
    Rect::from_center_size(pos2(0.5, 0.5), Vec2::new(w, h))
}

/// Drag handle `(hx, hy)` to `p`. Each of hx/hy is -1 (min edge), 1 (max edge) or 0 (untouched).
/// With `k` set, only corners may be dragged and the opposite corner stays put.
pub fn drag(r: Rect, hx: i8, hy: i8, p: Pos2, k: Option<f32>) -> Rect {
    let p = p.clamp(Pos2::ZERO, pos2(1.0, 1.0));
    let Some(k) = k else {
        let mut r = r;
        if hx < 0 {
            r.min.x = p.x.min(r.max.x - MIN)
        }
        if hx > 0 {
            r.max.x = p.x.max(r.min.x + MIN)
        }
        if hy < 0 {
            r.min.y = p.y.min(r.max.y - MIN)
        }
        if hy > 0 {
            r.max.y = p.y.max(r.min.y + MIN)
        }
        return r;
    };
    let a = pos2(
        if hx < 0 { r.max.x } else { r.min.x },
        if hy < 0 { r.max.y } else { r.min.y },
    );
    let room = Vec2::new(
        if hx < 0 { a.x } else { 1.0 - a.x },
        if hy < 0 { a.y } else { 1.0 - a.y },
    );
    let w = (p.x - a.x).abs().max((p.y - a.y).abs() * k).max(MIN);
    let w = w.min(room.x).min(room.y * k);
    Rect::from_two_pos(a, a + Vec2::new(w * f32::from(hx), w / k * f32::from(hy)))
}

/// Move `r` by `d`, staying inside the frame.
pub fn shift(r: Rect, d: Vec2) -> Rect {
    r.translate(Vec2::new(
        d.x.clamp(-r.min.x, 1.0 - r.max.x),
        d.y.clamp(-r.min.y, 1.0 - r.max.y),
    ))
}

/// Normalized rect → ffmpeg `crop` values `[w, h, x, y]` in an `fw`×`fh` frame, all even (yuv420).
pub fn to_pixels(r: Rect, fw: u32, fh: u32) -> [u32; 4] {
    let even = |v: f32| (v.round().max(0.0) as u32) & !1;
    let x = even(r.min.x * fw as f32).min(fw - 2);
    let y = even(r.min.y * fh as f32).min(fh - 2);
    let w = even(r.width() * fw as f32).clamp(2, (fw - x) & !1);
    let h = even(r.height() * fh as f32).clamp(2, (fh - y) & !1);
    [w, h, x, y]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_is_centered_and_inside() {
        let r = fit(lock(1.0, 16.0 / 9.0));
        assert!((r.width() - 9.0 / 16.0).abs() < 1e-6 && r.height() == 1.0);
        assert!((r.center().x - 0.5).abs() < 1e-6);
        assert_eq!(
            fit(2.0),
            Rect::from_min_max(pos2(0.0, 0.25), pos2(1.0, 0.75))
        );
    }

    #[test]
    fn free_drag_keeps_min_size() {
        let r = drag(full(), -1, 0, pos2(5.0, 0.3), None);
        assert!((r.width() - MIN).abs() < 1e-6 && r.min.y == 0.0);
    }

    #[test]
    fn locked_drag_keeps_ratio_and_bounds() {
        let k = 2.0;
        let r = drag(fit(k), 1, 1, pos2(0.9, 0.95), Some(k));
        assert!((r.width() / r.height() - k).abs() < 1e-4);
        assert!(r.max.x <= 1.0 + 1e-6 && r.max.y <= 1.0 + 1e-6);
        assert_eq!(r.min, fit(k).min, "opposite corner is anchored");
    }

    #[test]
    fn shift_clamps() {
        let r = Rect::from_min_max(pos2(0.1, 0.1), pos2(0.5, 0.5));
        assert_eq!(shift(r, Vec2::new(-1.0, 9.0)).min, pos2(0.0, 0.6));
    }

    #[test]
    fn pixels_are_even_and_inside() {
        assert_eq!(to_pixels(full(), 1920, 1080), [1920, 1080, 0, 0]);
        assert_eq!(to_pixels(full(), 1921, 1081), [1920, 1080, 0, 0]);
        let [w, h, x, y] = to_pixels(
            Rect::from_min_max(pos2(0.333, 0.5), pos2(1.0, 1.0)),
            1279,
            719,
        );
        assert!(w % 2 == 0 && h % 2 == 0 && x % 2 == 0 && y % 2 == 0);
        assert!(x + w <= 1279 && y + h <= 719);
    }
}
