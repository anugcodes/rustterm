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

/// JetBrains Mono Regular, bundled with the binary (SIL OFL 1.1, see
/// `assets/OFL.txt`). Regular weight, not bold.
const TERMINAL_FONT: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");

/// Install the bundled monospace font as the terminal's first choice.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "jetbrains_mono".to_owned(),
        egui::FontData::from_static(TERMINAL_FONT),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "jetbrains_mono".to_owned());
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
