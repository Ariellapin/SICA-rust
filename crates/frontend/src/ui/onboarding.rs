//! First-run onboarding (§7.3).
//!
//! A fresh checkout starts disconnected, with a blocked composer and no hint
//! that Settings › Models is where to go. This is that hint: one dialog,
//! once, offering the single thing that turns the app on.
//!
//! **It is not dsh's gate.** dsh is a hosted product where a key is
//! required, so it makes the app inert until one is entered. Here a local
//! provider — vLLM, llama.cpp — needs no key at all, and blocking a
//! local-only user behind a key field would be nagging them for something
//! they must not supply. So the dialog offers a key *and* says a local
//! server needs none, "Configure later" is a real answer that is remembered,
//! and nothing is inert either way: the composer behind it is the §6.8
//! blocked composer, which already explains itself.
//!
//! It appears when **nothing has ever worked**: no provider carries a key
//! (a `${VAR}` reference counts only if the variable resolves — §14.6) and
//! no provider was ever connected. Having connected once is proof enough
//! that the user knows where the setting lives.

use crate::app::App;
use crate::ui::kit::{self, Weight};

/// Should the dialog open at all? Pure so the rule can be tested without a
/// window, because the rule is the whole feature.
pub fn wanted(providers: &[crate::llm_providers::ProviderConfig], last_active: Option<&str>, dismissed: bool) -> bool {
    if dismissed || last_active.is_some() {
        return false;
    }
    !providers
        .iter()
        .any(|p| !sica_core::creds::resolve(&p.api_key).trim().is_empty())
}

/// Draw it when it is wanted. Returns nothing: every outcome is a mutation
/// on `App`, and the dialog closes itself.
pub fn draw(app: &mut App, ctx: &egui::Context) {
    if !app.onboarding_open {
        return;
    }
    let t = app.theme;
    let ids: Vec<String> = app.providers.iter().map(|p| p.id.clone()).collect();
    if ids.is_empty() {
        // Nothing to configure — the provider files are seeded at startup,
        // so this only happens if they were all deleted.
        app.onboarding_open = false;
        return;
    }
    let sel_id = egui::Id::new("onboarding_provider");
    let key_id = egui::Id::new("onboarding_key");
    let mut selected: usize = ctx.data(|d| d.get_temp(sel_id).unwrap_or(0)).min(ids.len() - 1);
    let mut key: String = ctx.data(|d| d.get_temp(key_id).unwrap_or_default());
    let mut save = false;
    let mut later = false;

    let out = kit::modal(
        ctx,
        egui::Id::new("onboarding_modal"),
        "Add an API key to get started",
        460.0,
        false,
        |ui| {
            kit::label(
                ui,
                kit::txt(
                    "Pick a provider and paste its key. A local server (vLLM, \
                     llama.cpp) needs no key — choose it and continue.",
                    13.0,
                    Weight::Regular,
                    kit::col(t.alias.label[1]),
                ),
            );
            ui.add_space(14.0);
            kit::label(
                ui,
                kit::txt("Provider", 12.0, Weight::Medium, kit::col(t.alias.label[1])),
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for (i, id) in ids.iter().enumerate() {
                    if kit::pill(ui, id, i == selected).clicked() {
                        selected = i;
                        ctx.data_mut(|d| d.insert_temp(sel_id, i));
                    }
                }
            });
            ui.add_space(12.0);
            kit::label(
                ui,
                kit::txt("API key", 12.0, Weight::Medium, kit::col(t.alias.label[1])),
            );
            ui.add_space(4.0);
            let masked = !key.starts_with("${");
            let field = egui::TextEdit::singleline(&mut key)
                .password(masked)
                .hint_text("paste a key, or ${MY_API_KEY}")
                .desired_width(ui.available_width());
            if ui.add(field).changed() {
                ctx.data_mut(|d| d.insert_temp(key_id, key.clone()));
            }
            ui.add_space(4.0);
            kit::label(
                ui,
                kit::txt(
                    "Stored in that provider's TOML under sica-settings/. \
                     ${VAR} keeps the secret in the environment instead.",
                    11.0,
                    Weight::Regular,
                    kit::col(t.alias.label[3]),
                ),
            );
            ui.add_space(16.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kit::button(ui, "Save and continue", kit::Variant::Primary, kit::Size::Md)
                    .clicked()
                {
                    save = true;
                }
                if kit::button(ui, "Configure later", kit::Variant::Ghost, kit::Size::Md).clicked()
                {
                    later = true;
                }
            });
        },
    );

    if save {
        let id = ids[selected].clone();
        if let Some(cfg) = app.providers.iter_mut().find(|p| p.id == id) {
            let typed = key.trim();
            if !typed.is_empty() {
                cfg.api_key = typed.to_string();
            }
        }
        // Connecting is the point of the dialog: the user came here to make
        // the app work, not to fill in a form. `connect_provider` saves the
        // TOML and records the provider as the last active one.
        app.connect_provider(&id);
    }
    if save || later || out.dismissed {
        app.onboarding_open = false;
        // Remembered either way — an offer declined is an answer, and asking
        // again on every start would be nagging.
        app.onboarded = true;
        app.persist_settings();
        ctx.data_mut(|d| d.insert_temp(key_id, String::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_providers::ProviderConfig;

    fn provider(id: &str, key: &str) -> ProviderConfig {
        let mut cfg = crate::llm_providers::defaults()
            .into_iter()
            .next()
            .expect("a seeded provider");
        cfg.id = id.into();
        cfg.api_key = key.into();
        cfg
    }

    #[test]
    fn it_offers_itself_only_when_nothing_has_ever_worked() {
        let none = vec![provider("local", ""), provider("openai", "")];
        assert!(wanted(&none, None, false));

        // A key anywhere is enough: the user has been here before.
        let keyed = vec![provider("local", ""), provider("openai", "sk-x")];
        assert!(!wanted(&keyed, None, false));

        // So is having connected — a local server needs no key at all, and
        // nagging that user for one would be asking for something they must
        // not supply.
        assert!(!wanted(&none, Some("local"), false));

        // And so is having said "later" once.
        assert!(!wanted(&none, None, true));
    }

    /// A `${VAR}` reference counts only when the variable actually resolves
    /// (§14.6) — an unset one is exactly the state this dialog is for.
    #[test]
    fn an_unresolved_reference_is_not_a_key() {
        let name = format!("SICA_ONBOARD_TEST_{}", std::process::id());
        let refs = vec![provider("openai", &format!("${{{name}}}"))];
        assert!(wanted(&refs, None, false));
        std::env::set_var(&name, "sk-from-env");
        assert!(!wanted(&refs, None, false));
        std::env::remove_var(&name);
    }
}
