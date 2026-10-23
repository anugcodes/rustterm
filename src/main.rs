mod app;
mod ssh;
mod terminal;

use app::RustTermApp;

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
            // Load a monospace font for the terminal.
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "mono".to_owned(),
                egui::FontData::from_static(include_bytes!(
                    "../assets/JetBrainsMono-Regular.ttf"
                ))
                .into(),
            );
            fonts
                .families
                .get_mut(&egui::FontFamily::Monospace)
                .unwrap()
                .insert(0, "mono".to_owned());
            cc.egui_ctx.set_fonts(fonts);

            Ok(Box::new(RustTermApp::new(cc)))
        }),
    )
}
