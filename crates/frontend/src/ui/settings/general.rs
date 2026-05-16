//! "General" settings — appearance, startup, logging, idealist policy,
//! skills folder. Each section opens with an italic-serif heading and a
//! hairline rule; the "why" subtext under each control is muted italic
//! serif.

use sica_core::paths::skills_dir;

use crate::app::App;
use crate::ui::widgets::{ghost_button, muted_italic, section_heading};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;

    // Appearance.
    section_heading(ui, &p, "Appearance");
    ui.horizontal(|ui| {
        let mut dark = app.theme_dark;
        ui.radio_value(&mut dark, true,  "Dark · iron");
        ui.add_space(12.0);
        ui.radio_value(&mut dark, false, "Light · paper");
        app.theme_dark = dark;
    });
    ui.add_space(4.0);
    ui.label(muted_italic(&p, "Theme changes apply when you press Apply."));

    ui.add_space(20.0);
    section_heading(ui, &p, "Startup");
    ui.checkbox(&mut app.auto_start_be, "Start BE service automatically");
    ui.checkbox(&mut app.auto_connect_llm, "Connect to LLM automatically once BE is up");

    ui.add_space(20.0);
    section_heading(ui, &p, "Logging");
    ui.checkbox(&mut app.log_raw_llm, "Log raw LLM responses to logs/model/");

    ui.add_space(20.0);
    section_heading(ui, &p, "Idealist");
    ui.checkbox(
        &mut app.idealist_auto_apply_be,
        "Auto-apply BE patches  (off by default)",
    );
    ui.add_space(4.0);
    ui.label(muted_italic(
        &p,
        "When off, idealist only writes Improvement-BE-*.md tickets. Frontend \
         issues always get a ticket and are never auto-patched.",
    ));

    ui.add_space(20.0);
    section_heading(ui, &p, "Skills");
    let dir = skills_dir();
    ui.label(muted_italic(
        &p,
        "Drop *.md files into this folder to register them as skills the \
         agent can call. Each file needs a `---` YAML frontmatter block with \
         `name:` and `description:`. The built-in `skill-creator` skill can \
         author new files for you. Changes are picked up on next BE restart.",
    ));
    ui.add_space(6.0);
    ui.label(format!("Folder: {}", dir.display()));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ghost_button(ui, &p, "Open skills folder").clicked() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(error = %e, "create skills dir failed");
            }
            if let Err(e) = open_in_explorer(&dir) {
                tracing::warn!(error = %e, "open skills folder failed");
            }
        }
        ui.add_space(8.0);
        if ghost_button(ui, &p, "Reveal skill-creator template").clicked() {
            if let Err(e) = agents::skill_creator::seed_default(&dir) {
                tracing::warn!(error = %e, "seed skill-creator failed");
            }
            let path = dir.join("skill-creator.md");
            if let Err(e) = open_in_explorer(&path) {
                tracing::warn!(error = %e, "open template failed");
            }
        }
    });

    let report = agents::md_skill::load_dir(&dir);
    ui.add_space(6.0);
    ui.label(muted_italic(
        &p,
        &format!("Detected {} skill file(s).", report.loaded.len()),
    ));
    for s in &report.loaded {
        ui.label(format!("  • {} — {}", s.name, s.description));
    }
    for (path, err) in &report.errors {
        ui.label(muted_italic(
            &p,
            &format!("  ✗ {}: {}", path.display(), err),
        ));
    }
}

/// Open a path in the OS file browser (Explorer on Windows, `open` on macOS,
/// `xdg-open` elsewhere). Best-effort — caller logs failures.
fn open_in_explorer(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    std::process::Command::new(program)
        .arg(path.as_os_str())
        .spawn()
        .map(|_| ())
}
