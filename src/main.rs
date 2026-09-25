#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod document;
mod platform;
mod search;
mod syntax;
mod terminal;
mod vim;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("twill")
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([600.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "twill",
        options,
        Box::new(|cc| Ok(Box::new(app::Twill::new(cc)))),
    )
}
