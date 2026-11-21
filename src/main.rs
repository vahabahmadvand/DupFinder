#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod models;
mod scanner;
mod ui;

use eframe::egui;

fn load_app_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).ok()
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("DupFinder — Ultra-Fast Duplicate File Finder")
        .with_inner_size([1060.0, 680.0])
        .with_min_inner_size([760.0, 480.0])
        .with_drag_and_drop(true);

    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "DupFinder",
        options,
        Box::new(|cc| Ok(Box::new(ui::DupFinderApp::new(cc)))),
    )
}
