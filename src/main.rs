#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
#[cfg(bundled)]
mod bundle;
mod crop;
mod ffmpeg;
mod player;

use eframe::egui;

fn main() -> eframe::Result {
    #[cfg(bundled)]
    if let Err(e) = bundle::unpack() {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("QPE")
            .set_description(format!("Can't unpack QPE's video libraries: {e}"))
            .show();
        std::process::exit(1);
    }
    let file = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("QPE")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/qpe.png"))
                    .expect("assets/qpe.png is a valid PNG"),
            )
            .with_inner_size([1040.0, 620.0])
            .with_min_inner_size([980.0, 320.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "QPE",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, file)?))),
    )
}
