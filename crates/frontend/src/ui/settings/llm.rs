//! LLM tab — one card per provider config (TOML file under
//! `sica-settings/llm-providers/`). A single backend `LlmClient` is shared,
//! so only one panel can be active at a time; the others show "idle".
//!
//! Panels render in a two-column grid; the active / last-connected provider
//! is pinned to the first slot so the user's working config is always
//! visible without scrolling.

use egui::Vec2;

use protocol::{LlmState, Request};

use sica_core::theme::Theme;

use crate::app::App;
use crate::supervisor::UiCommand;
use crate::ui::kit::{self, Size, Variant, Weight};

/// One card per provider, full width — the modal's content column is 588 px,
/// so a second column would squeeze the fields dsh gives a whole row.
const GRID_COLS: usize = 1;
const GRID_GUTTER: f32 = 12.0;

fn label_cell(ui: &mut egui::Ui, t: &Theme, text: &str) {
    ui.allocate_ui(Vec2::new(88.0, 22.0), |ui| {
        kit::label(
            ui,
            kit::txt(text, 12.0, Weight::Medium, kit::col(t.alias.label[2])),
        );
    });
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    ui.add_space(12.0);
    kit::label(
        ui,
        kit::txt(
            "Enter your API keys to use models from the following providers.",
            13.0,
            Weight::Regular,
            kit::col(t.alias.label[2]),
        ),
    );
    ui.add_space(10.0);

    if !app.ipc_state.connected {
        kit::label(
            ui,
            kit::txt(
                "The backend must be running to connect a model.",
                13.0,
                Weight::Regular,
                kit::col(t.alias.error),
            ),
        );
        ui.add_space(6.0);
    }

    if app.providers.is_empty() {
        kit::label(
            ui,
            kit::txt(
                "No provider configs found. Add TOML files under sica-settings/llm-providers/ and restart.",
                13.0,
                Weight::Regular,
                kit::col(t.alias.label[2]),
            ),
        );
        return;
    }

    // Draw order: active / last-connected provider first, the rest in their
    // natural (alphabetical) order. Pins the working panel to the top-left
    // so it greets the user without scrolling.
    let active_id = app.active_provider_id.clone();
    let mut order: Vec<usize> = (0..app.providers.len()).collect();
    if let Some(id) = active_id.as_deref() {
        if let Some(pos) = app.providers.iter().position(|cfg| cfg.id == id) {
            let chosen = order.remove(pos);
            order.insert(0, chosen);
        }
    }

    let mut clicked_connect: Option<String> = None;
    let mut clicked_disconnect = false;
    // Base URL whose model list the user asked for this frame.
    let mut clicked_fetch: Option<String> = None;

    let llm_state = app.llm_state.state.clone();
    let ipc_connected = app.ipc_state.connected;
    // Read-only snapshots: the loop below borrows `app.providers` mutably.
    let models = app.provider_models.clone();
    let pending = app.models_pending.clone();

    for row in order.chunks(GRID_COLS) {
        ui.columns(GRID_COLS, |ui_cols| {
            for (slot, &idx) in row.iter().enumerate() {
                let ui = &mut ui_cols[slot];
                let cfg = &mut app.providers[idx];
                let is_active = active_id.as_deref() == Some(cfg.id.as_str());
                let panel_state = if is_active { Some(&llm_state) } else { None };

                kit::card_frame(&t)
                    .inner_margin(egui::Margin::symmetric(16.0, 14.0))
                    .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        kit::label(
                            ui,
                            kit::txt(&cfg.title, 15.0, Weight::Medium, kit::col(t.alias.label[0])),
                        );
                        kit::label(ui, kit::mono(&cfg.id, 11.0, kit::col(t.alias.label[3])));
                    });
                    kit::label(
                        ui,
                        kit::txt(
                            &cfg.description,
                            12.0,
                            Weight::Regular,
                            kit::col(t.alias.label[2]),
                        ),
                    );
                    ui.add_space(10.0);

                    field_row(ui, &t, "Base URL", &mut cfg.base_url, false);
                    field_row(ui, &t, "Model",    &mut cfg.model,    false);
                    key_row(ui, &t, &mut cfg.api_key);
                    if models_row(ui, &t, cfg, &models, &pending, ipc_connected) {
                        clicked_fetch = Some(cfg.base_url.clone());
                    }

                    // Per-model recommendation matched from the model string.
                    // One click aligns temperature / thinking / tool mode;
                    // identity fields (URL, model, key) are never touched.
                    {
                        let preset = llm::preset::preset_for_provider(&cfg.base_url, &cfg.model);
                        let drift = cfg.preset_drift();
                        ui.horizontal(|ui| {
                            label_cell(ui, &t, "Preset");
                            kit::label(
                                ui,
                                kit::txt(
                                    format!("{}: {}", preset.label, preset.note),
                                    12.0,
                                    Weight::Regular,
                                    kit::col(t.alias.label[2]),
                                ),
                            )
                            .on_hover_text(preset.note);
                        });
                        if !drift.is_empty() {
                            ui.horizontal(|ui| {
                                label_cell(ui, &t, "Differs");
                                kit::label(
                                    ui,
                                    kit::txt(
                                        drift.join(", "),
                                        12.0,
                                        Weight::Regular,
                                        kit::col(t.alias.warn_label),
                                    ),
                                );
                                if kit::button(ui, "Apply preset", Variant::Outline, Size::Sm)
                                    .clicked()
                                {
                                    cfg.apply_preset(&preset);
                                    let _ = crate::llm_providers::save(cfg);
                                }
                            });
                        }
                    }

                    // Sampling / context tuning. 0 on the token fields means
                    // "auto" (server default / auto-detect) — see hover text.
                    ui.horizontal(|ui| {
                        label_cell(ui, &t, "Temp");
                        ui.add(
                            egui::DragValue::new(&mut cfg.temperature)
                                .range(0.0..=2.0)
                                .speed(0.01)
                                .fixed_decimals(2),
                        )
                        .on_hover_text(
                            "Sampling temperature. Low (0.1–0.3) keeps tool \
                             calls and factual answers precise; higher adds \
                             variety.",
                        );
                        ui.add_space(10.0);
                        label_cell(ui, &t, "Max tok");
                        ui.add(
                            egui::DragValue::new(&mut cfg.max_tokens)
                                .range(0..=262_144)
                                .speed(64),
                        )
                        .on_hover_text(
                            "Per-response completion cap. 0 = server default.",
                        );
                        ui.add_space(10.0);
                        label_cell(ui, &t, "Ctx");
                        ui.add(
                            egui::DragValue::new(&mut cfg.context_window)
                                .range(0..=1_048_576)
                                .speed(256),
                        )
                        .on_hover_text(
                            "Prompt-window budget for history trimming. \
                             0 = auto-detect from the server — llama.cpp \
                             reports its launched --ctx-size via /props \
                             (falls back to 24k).",
                        );
                    });
                    ui.add_space(4.0);
                    ui.checkbox(
                        &mut cfg.native_tools,
                        "Native tool calling (OpenAI tools API)",
                    )
                    .on_hover_text(
                        "Send skills as OpenAI `tools` and read `tool_calls` \
                         from the response instead of the text protocol. \
                         Requires server-side tool support — e.g. vLLM started \
                         with --enable-auto-tool-choice and a --tool-call-parser \
                         matching the model. Takes effect on next Connect.",
                    );
                    ui.add_enabled_ui(cfg.native_tools, |ui| {
                        ui.checkbox(
                            &mut cfg.ptc,
                            "Programmatic tool calling (run-code)",
                        )
                        .on_hover_text(
                            "Offer the model one data tool, run-code, and let                              it call every other skill from inside a sandboxed                              Rhai program. Collapses a whole read/filter/edit                              sequence into one round-trip and keeps the                              intermediate data out of the context — only what                              the program prints comes back. Needs native tool                              calling, and a model good enough to write the                              script. Takes effect on next Connect.",
                        );
                    });
                    if cfg.ptc && !cfg.native_tools {
                        // The wire has no way to say "PTC over the text
                        // protocol", so a stale flag would silently mean
                        // plain text mode. Clear it where the user can see.
                        cfg.ptc = false;
                    }
                    ui.checkbox(
                        &mut cfg.thinking,
                        "Thinking (model reasoning)",
                    )
                    .on_hover_text(
                        "Let the model emit <think> reasoning before its \
                         answer. Off sends chat_template_kwargs \
                         {\"enable_thinking\": false}, which llama.cpp/vLLM \
                         chat templates honour for models like Qwen3 — \
                         faster, terser replies. Providers that ignore the \
                         field keep reasoning on. Takes effect on next \
                         Connect.",
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        label_cell(ui, &t, "Compact at");
                        ui.add(
                            egui::DragValue::new(&mut cfg.compact_threshold_pct)
                                .range(0..=99)
                                .speed(1),
                        )
                        .on_hover_text(
                            "Fold older history into an LLM-written summary \
                             when the prompt reaches this % of the budget. \
                             0 = default (80).",
                        );
                        ui.add_space(6.0);
                        label_cell(ui, &t, "Keep tail");
                        ui.add(
                            egui::DragValue::new(&mut cfg.compact_retain_pct)
                                .range(0..=90)
                                .speed(1),
                        )
                        .on_hover_text(
                            "Share of the budget (%) kept verbatim as the \
                             tail when compacting. 0 = default (16).",
                        );
                        ui.add_space(6.0);
                        label_cell(ui, &t, "Sum tok");
                        ui.add(
                            egui::DragValue::new(&mut cfg.compact_max_tokens)
                                .range(0..=65_536)
                                .speed(64),
                        )
                        .on_hover_text(
                            "Completion cap for the compaction summary. \
                             A summary cut off by it is discarded and \
                             retried. 0 = default (8192).",
                        );
                    });

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        let panel_connecting = matches!(panel_state, Some(LlmState::Connecting));
                        let panel_ready      = matches!(panel_state, Some(LlmState::Ready { .. }));
                        let connect_enabled  = ipc_connected && !panel_connecting && !panel_ready;
                        let connect_label    = if panel_connecting { "Connecting" } else { "Connect" };

                        let connect_resp = kit::button_enabled(
                            ui,
                            connect_label,
                            Variant::Primary,
                            Size::Sm,
                            connect_enabled,
                        );
                        if !connect_enabled {
                            let hint = if !ipc_connected {
                                "BE service must be running first."
                            } else if panel_connecting {
                                "Already connecting — wait for the request to finish."
                            } else {
                                "Already connected. Click Disconnect first to reconnect."
                            };
                            connect_resp.clone().on_hover_text(hint);
                        }
                        if connect_resp.clicked() {
                            clicked_connect = Some(cfg.id.clone());
                        }

                        let disc_enabled = is_active
                            && matches!(panel_state, Some(LlmState::Ready { .. } | LlmState::Connecting));
                        if kit::button_enabled(
                            ui,
                            "Disconnect",
                            Variant::Outline,
                            Size::Sm,
                            disc_enabled,
                        )
                        .clicked()
                        {
                            clicked_disconnect = true;
                        }

                        ui.add_space(12.0);
                        draw_status(ui, &t, panel_state);
                    });
                });
            }
        });
        ui.add_space(GRID_GUTTER);
    }

    if let Some(base_url) = clicked_fetch {
        let api_key = app
            .providers
            .iter()
            .find(|c| c.base_url == base_url)
            .map(|c| c.api_key.clone())
            .filter(|k| !k.is_empty());
        app.models_pending.insert(base_url.clone());
        app.send(UiCommand::SendRequest(Request::ListModels { base_url, api_key }));
    }
    if let Some(id) = clicked_connect {
        app.connect_provider(&id);
    } else if clicked_disconnect {
        app.send(UiCommand::SendRequest(Request::DisconnectLlm));
        app.active_provider_id = None;
        app.persist_settings();
    }

    ui.add_space(4.0);
    if kit::button(ui, "Open provider folder", Variant::Outline, Size::Sm)
        .on_hover_text("One TOML per provider; the filename stem is its id")
        .clicked()
    {
        let dir = sica_core::paths::llm_providers_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = super::open_path(&dir);
    }
    ui.add_space(24.0);
}

/// "Fetch available models" (7): a button that asks the provider what it
/// serves, and the answer as pickable chips. Returns `true` when the user
/// asked for a fetch — the request goes out after the loop, which is where
/// `app` is free to be borrowed again.
fn models_row(
    ui: &mut egui::Ui,
    t: &Theme,
    cfg: &mut crate::llm_providers::ProviderConfig,
    models: &std::collections::HashMap<String, Result<Vec<String>, String>>,
    pending: &std::collections::HashSet<String>,
    ipc_connected: bool,
) -> bool {
    let mut fetch = false;
    let busy = pending.contains(&cfg.base_url);
    ui.horizontal(|ui| {
        label_cell(ui, t, "");
        let label = if busy { "Fetching…" } else { "Fetch available models" };
        if kit::button_enabled(
            ui,
            label,
            Variant::Outline,
            Size::Sm,
            ipc_connected && !busy && !cfg.base_url.trim().is_empty(),
        )
        .on_hover_text("GET /v1/models on this provider")
        .clicked()
        {
            fetch = true;
        }
    });
    match models.get(&cfg.base_url) {
        Some(Err(e)) => {
            ui.horizontal(|ui| {
                label_cell(ui, t, "");
                kit::label(
                    ui,
                    kit::txt(
                        kit::one_line(e, 90),
                        12.0,
                        Weight::Regular,
                        kit::col(t.alias.error),
                    ),
                );
            });
        }
        Some(Ok(list)) if list.is_empty() => {
            ui.horizontal(|ui| {
                label_cell(ui, t, "");
                kit::label(
                    ui,
                    kit::txt(
                        "The provider reported no models.",
                        12.0,
                        Weight::Regular,
                        kit::col(t.alias.label[2]),
                    ),
                );
            });
        }
        Some(Ok(list)) => {
            let mut picked: Option<String> = None;
            ui.horizontal_wrapped(|ui| {
                ui.add_space(88.0);
                for m in list {
                    if kit::pill(ui, m, *m == cfg.model).clicked() {
                        picked = Some(m.clone());
                    }
                }
            });
            if let Some(m) = picked {
                cfg.model = m;
                let _ = crate::llm_providers::save(cfg);
            }
        }
        None => {}
    }
    ui.add_space(2.0);
    fetch
}

/// The API-key row (§14.6). The value is **write-only**: what is on screen
/// is a state — configured, and from where — never the key. A key that is
/// already stored stays stored unless something is typed over it, and a
/// `${VAR}` reference is shown as itself, because a variable name is not a
/// secret and hiding it would only make it unfixable.
fn key_row(ui: &mut egui::Ui, t: &Theme, value: &mut String) {
    let reference = sica_core::creds::is_reference(value);
    let status = match sica_core::creds::describe(value) {
        sica_core::creds::Status::Absent => "not set".to_string(),
        sica_core::creds::Status::Configured(sica_core::creds::Source::Literal) => {
            "configured · in file".to_string()
        }
        sica_core::creds::Status::Configured(sica_core::creds::Source::Env(name)) => {
            format!("configured · environment ({name})")
        }
        sica_core::creds::Status::Configured(sica_core::creds::Source::File(name)) => {
            format!("configured · sica-settings/.env ({name})")
        }
        sica_core::creds::Status::Unresolved(name) => format!("{name} is not set"),
    };
    ui.horizontal(|ui| {
        label_cell(ui, t, "API key");
        let w = (ui.available_width() - 4.0).max(80.0);
        // A reference is readable; a literal key never is.
        let edit = egui::TextEdit::singleline(value)
            .password(!reference)
            .hint_text("key or ${VAR}")
            .desired_width(w);
        ui.add(edit);
    });
    ui.horizontal(|ui| {
        label_cell(ui, t, "");
        kit::label(
            ui,
            kit::txt(status, 11.0, Weight::Regular, kit::col(t.alias.label[3])),
        );
    });
    ui.add_space(4.0);
}

fn field_row(
    ui: &mut egui::Ui,
    t: &Theme,
    label: &str,
    value: &mut String,
    password: bool,
) {
    ui.horizontal(|ui| {
        label_cell(ui, t, label);
        // Fill the remaining card width so the input scales with the grid
        // cell rather than being clipped by fixed widths from the old
        // single-column layout.
        let w = (ui.available_width() - 4.0).max(80.0);
        let mut edit = egui::TextEdit::singleline(value).desired_width(w);
        if password {
            edit = edit.password(true);
        }
        ui.add(edit);
    });
    ui.add_space(4.0);
}

fn draw_status(ui: &mut egui::Ui, t: &Theme, state: Option<&LlmState>) {
    let (text, fill, fg) = match state {
        Some(LlmState::Connecting) => (
            "Connecting",
            kit::col(t.alias.warn_tertiary),
            kit::col(t.alias.warn_label),
        ),
        Some(LlmState::Ready { model, .. }) => {
            kit::tinted_pill(
                ui,
                "Ready",
                kit::col(t.alias.success_tertiary),
                kit::col(t.alias.success),
                11.0,
            );
            ui.add_space(6.0);
            kit::label(ui, kit::mono(model, 11.0, kit::col(t.alias.label[2])));
            return;
        }
        Some(LlmState::Error { message }) => {
            kit::tinted_pill(
                ui,
                "Error",
                kit::col(t.alias.error).linear_multiply(if t.dark { 0.35 } else { 0.14 }),
                kit::col(t.alias.error),
                11.0,
            );
            ui.add_space(6.0);
            kit::label(
                ui,
                kit::txt(message, 12.0, Weight::Regular, kit::col(t.alias.error)),
            );
            return;
        }
        Some(LlmState::Disconnected) | None => (
            "Idle",
            kit::cola(t.alias.hover),
            kit::col(t.alias.label[2]),
        ),
    };
    kit::tinted_pill(ui, text, fill, fg, 11.0);
}
