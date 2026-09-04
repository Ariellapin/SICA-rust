//! The details column (§2.1). dsh hosts one slot here,
//! `conversation.details.tool`: the selected tool call's full payload. The
//! column is closed (`details_w == 0`) until a row asks for it.
//!
//! Two things ask. From the transcript it is a tool row, and the column shows
//! that call. From the Trajectory view (§10) it is a ledger row, and the
//! column becomes the **event inspector** — Summary · Payload · Result ·
//! Timing · Raw, over the durable event rather than the live chip.
//!
//! dsh's remaining tabs — Schema, System Prompt, Tools, Options — read the
//! *request envelope*: the composed system prompt, the tool schemas the
//! `tools` array carried, and the sampling options one request went out
//! with. The log records one (`EventKind::RequestEnvelope`) whenever the
//! envelope *changes*, and every row names the newest one at or before it,
//! so these four tabs show what the model was reading at that row rather
//! than what it would read now. A row from before the session's first
//! request — or from a log written by a backend that recorded none — says so
//! instead of showing today's prompt and calling it history.

use protocol::{EnvelopeDump, EventDump, EventTag};

use crate::app::{App, ChatView};
use crate::ui::kit::{self, Weight};

/// Inspector tabs, in dsh's order minus Diff / Source / Usage, whose content
/// lives elsewhere in this app (the tool row's diff body, the ledger's own
/// token columns).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Summary,
    Payload,
    Result,
    /// The selected tool's own JSON schema, cut out of the envelope's tools
    /// array. Offered only on a tool row, where it names something.
    Schema,
    Timing,
    SystemPrompt,
    Tools,
    Options,
    Raw,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Summary => "Summary",
            Tab::Payload => "Payload",
            Tab::Result => "Result",
            Tab::Schema => "Schema",
            Tab::Timing => "Timing",
            Tab::SystemPrompt => "System Prompt",
            Tab::Tools => "Tools",
            Tab::Options => "Options",
            Tab::Raw => "Raw",
        }
    }

    /// Which tabs this row offers. Schema appears only on a tool row: on any
    /// other event there is no tool whose schema it could be, and a tab that
    /// is always empty teaches the reader to stop clicking it.
    fn for_row(ev: &EventDump) -> Vec<Tab> {
        let mut tabs = vec![Tab::Summary, Tab::Payload, Tab::Result];
        if matches!(ev.tag, EventTag::Tool | EventTag::ToolResult) {
            tabs.push(Tab::Schema);
        }
        tabs.extend([
            Tab::Timing,
            Tab::SystemPrompt,
            Tab::Tools,
            Tab::Options,
            Tab::Raw,
        ]);
        tabs
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
    let tabs = Tab::for_row(&ev);
    let mut tab: Tab = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(Tab::Summary);
    // Selecting a Schema row and then clicking a non-tool one would otherwise
    // leave the inspector on a tab this row does not offer.
    if !tabs.contains(&tab) {
        tab = Tab::Summary;
    }
    ui.horizontal_wrapped(|ui| {
        for candidate in tabs {
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
        .show(ui, |ui| {
            let env = super::trajectory::envelope_of(&app.trajectory, &ev).cloned();
            match tab {
                Tab::Summary => summary_tab(app, ui, &ev),
                Tab::Payload => body_tab(ui, &ev.payload, "This event carries no payload."),
                Tab::Result => body_tab(ui, &ev.result, "This event carries no result."),
                Tab::Schema => schema_tab(ui, &ev, env.as_ref()),
                Tab::Timing => timing_tab(app, ui, &ev),
                Tab::SystemPrompt => envelope_tab(ui, env.as_ref(), |e| (&e.system, "text")),
                Tab::Tools => envelope_tab(ui, env.as_ref(), |e| (&e.tools, "json")),
                Tab::Options => envelope_tab(ui, env.as_ref(), |e| (&e.options, "json")),
                Tab::Raw => kit::code_block(ui, "json", &ev.raw),
            }
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
    // Which prompt was in force here. Two rows carrying the same envelope
    // seq went out with byte-identical system prompts, tools and options,
    // which is the fact the four envelope tabs rest on.
    facts.push((
        "Envelope".into(),
        match ev.envelope {
            Some(seq) => format!("#{seq}"),
            None => "none recorded".into(),
        },
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

/// One body out of the envelope in force at the row, or the reason there is
/// none. The two "none" cases are different and are worth saying apart: a row
/// with no envelope at all predates the session's first request, while an
/// envelope whose body is empty is a real answer (text-protocol mode carries
/// no tools array, and the catalogue is in the system prompt instead).
fn envelope_tab<'a>(
    ui: &mut egui::Ui,
    env: Option<&'a EnvelopeDump>,
    pick: impl Fn(&'a EnvelopeDump) -> (&'a String, &'static str),
) {
    let Some(env) = env else {
        empty(
            ui,
            "No request envelope covers this event \u{2014} it happened before the \
             session's first request, or the log was written by a backend that \
             did not record one.",
        );
        return;
    };
    let (body, lang) = pick(env);
    if body.trim().is_empty() {
        empty(
            ui,
            "Empty in this envelope. In text-protocol mode the tools array is \
             not sent at all \u{2014} the catalogue is a section of the system \
             prompt instead.",
        );
        return;
    }
    kit::footnote(ui, &format!("from the envelope at #{}", env.seq));
    ui.add_space(4.0);
    kit::code_block(ui, lang, body);
}

/// The selected tool's entry in the envelope's tools array. Native mode only:
/// under the text protocol there is no schema, because the model is told
/// about the tool in prose.
fn schema_tab(ui: &mut egui::Ui, ev: &EventDump, env: Option<&EnvelopeDump>) {
    let Some(name) = tool_name_of(ev) else {
        empty(ui, "This row names no tool.");
        return;
    };
    let Some(env) = env else {
        empty(ui, "No request envelope covers this event, so there is no schema to show.");
        return;
    };
    if env.tools.trim().is_empty() {
        empty(
            ui,
            "This request went out under the text protocol, which sends no tool \
             schemas \u{2014} the model reads the catalogue in the system prompt. \
             Look there instead.",
        );
        return;
    }
    match find_schema(&env.tools, &name) {
        Some(schema) => {
            kit::footnote(ui, &format!("{name} \u{b7} from the envelope at #{}", env.seq));
            ui.add_space(4.0);
            kit::code_block(ui, "json", &schema);
        }
        None => empty(
            ui,
            &format!(
                "`{name}` is not in the tools array this request carried. A skill \
                 registered after the envelope was recorded, or one excluded from \
                 this call, looks exactly like this."
            ),
        ),
    }
}

/// The skill a tool row is about. The ledger's text opens with the name for
/// both halves of a call: `read-file 'x'` and `read-file \u{b7} contents`.
fn tool_name_of(ev: &EventDump) -> Option<String> {
    if !matches!(ev.tag, EventTag::Tool | EventTag::ToolResult) {
        return None;
    }
    let head = ev.text.split(['\u{b7}', ' ']).next()?.trim();
    (!head.is_empty()).then(|| head.to_string())
}

/// Cut one function's schema out of an OpenAI `tools` array.
fn find_schema(tools_json: &str, name: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(tools_json).ok()?;
    let found = parsed.as_array()?.iter().find(|t| {
        t.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            == Some(name)
    })?;
    serde_json::to_string_pretty(found).ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_row(tag: EventTag, text: &str) -> EventDump {
        EventDump {
            seq: 1,
            ts: 0,
            tag,
            text: text.into(),
            payload: String::new(),
            result: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            ok: None,
            shadowed: false,
            shadows: None,
            call_seq: None,
            turn_id: None,
            raw: String::new(),
            envelope: None,
        }
    }

    #[test]
    fn schema_is_offered_on_tool_rows_only() {
        assert!(Tab::for_row(&tool_row(EventTag::Tool, "read-file 'x'")).contains(&Tab::Schema));
        assert!(Tab::for_row(&tool_row(EventTag::ToolResult, "read-file \u{b7} ok")).contains(&Tab::Schema));
        assert!(!Tab::for_row(&tool_row(EventTag::User, "hello")).contains(&Tab::Schema));
        // The envelope tabs are on every row: "no envelope here" is itself
        // something the reader needs to be able to find out.
        assert!(Tab::for_row(&tool_row(EventTag::User, "hi")).contains(&Tab::SystemPrompt));
    }

    #[test]
    fn the_tool_name_is_read_off_either_half_of_a_call() {
        assert_eq!(
            tool_name_of(&tool_row(EventTag::Tool, "read-file 'README.md'")).as_deref(),
            Some("read-file")
        );
        assert_eq!(
            tool_name_of(&tool_row(EventTag::ToolResult, "run-cli \u{b7} exit 0")).as_deref(),
            Some("run-cli")
        );
        assert_eq!(tool_name_of(&tool_row(EventTag::Usage, "1200 tok")), None);
    }

    #[test]
    fn a_schema_is_cut_out_of_the_tools_array_by_name() {
        let tools = r#"[
            {"type":"function","function":{"name":"glob","parameters":{}}},
            {"type":"function","function":{"name":"read-file","parameters":{"x":1}}}
        ]"#;
        let found = find_schema(tools, "read-file").unwrap();
        assert!(found.contains("read-file"));
        assert!(!found.contains("glob"));
        assert!(find_schema(tools, "write-file").is_none());
        assert!(find_schema("not json", "glob").is_none());
    }
}
