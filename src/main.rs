mod app;
mod input;
mod persist;
mod secrets;
mod sessions;
mod settings;
mod sftp;
mod ssh;
mod terminal;
mod theme;
mod x11;

use app::RustTermApp;

/// Load an optional monospace font at runtime.
///
/// Using `std::fs::read` instead of `include_bytes!` means the project
/// compiles even if the font is absent; we simply fall back to egui's
/// built-in monospace family.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let Some((path, bytes)) = font_candidates()
        .into_iter()
        .find_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
    else {
        // The font is optional, so a missing file is not worth a warning.
        log::debug!("No bundled terminal font found; using egui's default monospace.");
        ctx.set_fonts(fonts);
        return;
    };

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

    ctx.set_fonts(fonts);
}

/// Where a bundled font may live: next to the working directory (running from
/// the source tree) or next to the executable (installed layout).
fn font_candidates() -> Vec<std::path::PathBuf> {
    let mut paths = vec![std::path::PathBuf::from("assets/JetBrainsMono-Regular.ttf")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("assets/JetBrainsMono-Regular.ttf"));
        }
    }
    paths
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
