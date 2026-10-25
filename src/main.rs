mod app;
mod input;
mod ssh;
mod terminal;

use app::RustTermApp;

/// Load an optional monospace font at runtime.
///
/// Using `std::fs::read` instead of `include_bytes!` means the project
/// compiles even if the font is absent; we simply fall back to egui's
/// built-in monospace family.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let path = std::path::Path::new("assets/JetBrainsMono-Regular.ttf");
    match std::fs::read(path) {
        Ok(bytes) => {
            fonts.font_data.insert(
                "jetbrains_mono".to_owned(),
                egui::FontData::from_owned(bytes),
            );
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "jetbrains_mono".to_owned());
            log::info!("Loaded terminal font from {}", path.display());
        }
        Err(_) => {
            log::warn!(
                "No font at {}; using egui's default monospace.",
                path.display()
            );
        }
    }

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([700.0, 400.0])
            .with_title("rustterm"),
        ..Default::default()
    };

    eframe::run_native(
        "rustterm",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(RustTermApp::new(cc)))
        }),
    )
}
