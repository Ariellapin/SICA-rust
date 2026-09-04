//! The details column (§2.1). dsh hosts one slot here,
//! `conversation.details.tool`: the selected tool call's full payload. The
//! column is closed (`details_w == 0`) until a row asks for it.
//!
//! Two things ask. From the transcript it is a tool row, and the column shows
//! that call. From the Trajectory view (§10) it is a ledger row, and the
//! column becomes the **event inspector** — Summary · Payload · Result ·
//! Timing · Raw, over the durable event rather than the live chip.
//!
//! dsh's remaining tabs (Schema, System Prompt, Tools, Options) need the
//! *request envelope* — the system prompt and tool schemas a request went out
//! with. `TokenUsage.breakdown` prices those three sections but the log does
//! not store them, so those tabs would have nothing truthful to show and are
//! left out until `TurnStart` carries a `RequestEnvelope`.

use protocol::EventDump;

use crate::app::{App, ChatView};
use crate::ui::kit::{self, Weight};

/// Inspector tabs, in dsh's order minus the ones with no durable source.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Summary,
    Payload,
    Result,
    Timing,
    Raw,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Summary, Tab::Payload, Tab::Result, Tab::Timing, Tab::Raw];
    fn label(self) -> &'static str {
        match self {
            Tab::Summary => "Summary",
            Tab::Payload => "Payload",
            Tab::Result => "Result",
            Tab::Timing => "Timing",
            Tab::Raw => "Raw",
        }
    }
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let event_view = app.view == ChatView::Trajectory;
    ui.horizontal(|ui| {
        kit::label(
            ui,
            kit::txt(
                if event_view { "Event" } else { "Tool details" },
                14.0,
                Weight::Medium,
                kit::col(t.alias.label[0]),
            ),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if kit::icon_button(ui, crate::ui::icons::Icon::Close, 28.0).clicked() {
                app.layout.details_w = 0.0;
                app.details_call = None;
                app.trajectory.selected = None;
            }
        });
    });
    ui.add_space(8.0);

    if event_view {
        event_inspector(app, ui);
    } else {
        tool_details(app, ui);
    }
}

// ---------------------------------------------------------------------------
// Trajectory: the event inspector
// ---------------------------------------------------------------------------

fn event_inspector(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let Some(ev) = super::trajectory::selected_row(&app.trajectory).cloned() else {
        empty(ui, "Click a row in the ledger to inspect the event.");
        return;
    };

    // Header: the kind, the seq, and — the reason the view exists — whether
    // the fold still shows this event to the model.
    ui.horizontal(|ui| {
        kit::tinted_pill(
            ui,
            ev.tag.label(),
            kit::col(t.alias.tip),
            kit::col(t.alias.label[1]),
            11.0,
        );
        kit::label(
            ui,
            kit::mono(format!("#{}", ev.seq), 12.0, kit::col(t.alias.label[2])),
        );
        if ev.shadowed {
            kit::tinted_pill(
                ui,
                "shadowed",
                kit::col(t.alias.warn_tertiary),
                kit::col(t.alias.warn_label),
                11.0,
            )
            .on_hover_text(
                "Still in the log, but a later compaction or rewind took it out of \
                 the model's view.",
            );
        }
    });
    ui.add_space(6.0);
    kit::label(
        ui,
        kit::txt(&ev.text, 13.0, Weight::Regular, kit::col(t.alias.label[0])),
    );
    ui.add_space(10.0);

    // Tabs. Held in egui memory rather than on `App`: the choice is per
    // inspector and means nothing once the column is closed.
    let id = egui::Id::new("event_inspector_tab");
    let mut tab: Tab = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(Tab::Summary);
    ui.horizontal_wrapped(|ui| {
        for candidate in Tab::ALL {
            if kit::pill(ui, candidate.label(), tab == candidate).clicked() {
                tab = candidate;
            }
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(id, tab));
    ui.add_space(8.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .id_source("event_inspector_body")
        .show(ui, |ui| match tab {
            Tab::Summary => summary_tab(app, ui, &ev),
            Tab::Payload => body_tab(ui, &ev.payload, "This event carries no payload."),
            Tab::Result => body_tab(ui, &ev.result, "This event carries no result."),
            Tab::Timing => timing_tab(app, ui, &ev),
            Tab::Raw => kit::code_block(ui, "json", &ev.raw),
        });
}

fn summary_tab(app: &mut App, ui: &mut egui::Ui, ev: &EventDump) {
    let t = app.theme;
    let mut facts: Vec<(String, String)> = vec![
        ("Seq".into(), ev.seq.to_string()),
        ("Kind".into(), ev.tag.label().to_string()),
        ("At".into(), super::trajectory::stamp(ev.ts)),
    ];
    if let Some(turn) = ev.turn_id {
        facts.push(("Turn".into(), turn.to_string()));
    }
    if let Some(ok) = ev.ok {
        facts.push(("Outcome".into(), if ok { "ok" } else { "failed" }.into()));
    }
    if ev.tokens_in > 0 || ev.tokens_out > 0 {
        facts.push((
            "Tokens".into(),
            format!(
                "in {} · out {}",
                super::trajectory::thousands(ev.tokens_in),
                super::trajectory::thousands(ev.tokens_out)
            ),
        ));
    }
    if let Some((start, end)) = ev.shadows {
        facts.push(("Shadows".into(), format!("#{start} – #{end}")));
    }
    facts.push((
        "In the model's view".into(),
        if ev.shadowed { "no" } else { "yes" }.into(),
    ));
    for (k, v) in facts {
        fact_row(ui, &t, &k, &v, 130.0);
    }

    // The join a tool result carries: jump to the call that produced it.
    if let Some(call) = ev.call_seq {
        ui.add_space(10.0);
        if kit::button(
            ui,
            &format!("Go to the call (#{call})"),
            kit::Variant::Outline,
            kit::Size::Sm,
        )
        .clicked()
        {
            app.trajectory.selected = Some(call);
            app.trajectory.scroll_to = Some(call);
        }
    }
}

fn timing_tab(app: &mut App, ui: &mut egui::Ui, ev: &EventDump) {
    let t = app.theme;
    let rows = &app.trajectory.rows;
    let first = rows.first().map(|r| r.ts).unwrap_or(ev.ts);
    let prev = rows
        .iter()
        .take_while(|r| r.seq < ev.seq)
        .last()
        .map(|r| r.ts);
    let next = rows.iter().find(|r| r.seq > ev.seq).map(|r| r.ts);
    // The log stores a timestamp per event and no durations, so every span
    // here is a difference between two stamps — named as such rather than
    // presented as a measured duration.
    let mut facts = vec![
        ("Wall clock".to_string(), super::trajectory::stamp(ev.ts)),
        (
            "Since the first event".into(),
            super::trajectory::duration(ev.ts - first),
        ),
    ];
    if let Some(p) = prev {
        facts.push((
            "Since the previous".into(),
            super::trajectory::duration(ev.ts - p),
        ));
    }
    if let Some(n) = next {
        facts.push((
            "Until the next".into(),
            super::trajectory::duration(n - ev.ts),
        ));
    }
    for (k, v) in facts {
        fact_row(ui, &t, &k, &v, 160.0);
    }
}

/// One `key   value` line: a fixed-width key gutter so the values line up
/// down the panel, the value in mono because every one of them is a number,
/// a stamp or an id.
fn fact_row(
    ui: &mut egui::Ui,
    t: &sica_core::theme::Theme,
    key: &str,
    value: &str,
    gutter: f32,
) {
    ui.horizontal(|ui| {
        ui.set_min_height(22.0);
        ui.allocate_ui_with_layout(
            egui::Vec2::new(gutter, 18.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                kit::label(
                    ui,
                    kit::txt(key, 12.0, Weight::Regular, kit::col(t.alias.label[2])),
                );
            },
        );
        kit::label(ui, kit::mono(value, 12.0, kit::col(t.alias.label[0])));
    });
}

fn body_tab(ui: &mut egui::Ui, text: &str, empty_text: &str) {
    if text.trim().is_empty() {
        empty(ui, empty_text);
        return;
    }
    // JSON gets the code block's mono treatment either way; the language tag
    // only names what it is when it really parses.
    let lang = if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        "json"
    } else {
        "text"
    };
    kit::code_block(ui, lang, text);
}

fn empty(ui: &mut egui::Ui, text: &str) {
    let t = kit::theme(ui);
    kit::label(
        ui,
        kit::txt(text, 13.0, Weight::Regular, kit::col(t.alias.label[2])),
    );
}

// ---------------------------------------------------------------------------
// Chat: the selected tool call
// ---------------------------------------------------------------------------

fn tool_details(app: &mut App, ui: &mut egui::Ui) {
    let t = app.theme;
    let selected = app.details_call.and_then(|id| {
        app.chat
            .turns
            .iter()
            .flat_map(|t| t.tool_chips.iter())
            .find(|c| c.id == id)
            .cloned()
    });
    let Some(chip) = selected else {
        empty(ui, "Click a tool row in the message flow to view its details.");
        return;
    };

    kit::label(
        ui,
        kit::txt(
            super::tool_row::title_of(&chip.name),
            13.0,
            Weight::Medium,
            kit::col(t.alias.label[1]),
        ),
    );
    kit::footnote(ui, &chip.name);
    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            kit::io_card(ui, &chip.args_preview, &chip.summary, !chip.ok);
            if !chip.expectation.is_empty() {
                ui.add_space(6.0);
                kit::footnote(ui, &format!("expected: {}", chip.expectation));
            }
        });
}
