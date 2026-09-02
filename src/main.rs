mod app;
mod app_shell_foundation;
mod localization;
mod markdown;
mod persistence;
mod ui;

use app::TacetaApp;
use eframe::egui;

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Taceta")
            .with_inner_size([1_280.0, 820.0])
            .with_min_inner_size([900.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Taceta",
        native_options,
        Box::new(|creation_context| Ok(Box::new(TacetaApp::new(creation_context)))),
    )
}
