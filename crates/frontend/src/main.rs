mod app;
mod builder;
mod child;
mod icon;
mod ipc_client;
mod llm_providers;
mod settings_store;
mod supervisor;
mod ui;
mod watcher;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("sica-rust")
            .with_icon(std::sync::Arc::new(icon::generate())),
        // eframe would otherwise push `Visuals::light()/dark()` into the
        // context whenever the OS theme is read or changes, wiping the token
        // set `App::apply_visuals` installed. The Appearance preference owns
        // the theme; `ThemeMode::System` reads the OS preference itself from
        // `CreationContext::integration_info`.
        follow_system_theme: false,
        ..Default::default()
    };

    eframe::run_native(
        "sica-rust",
        native_options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
