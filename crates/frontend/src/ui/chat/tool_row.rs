//! Tool calls as **rows, not cards** (§3.4).
//!
//! `[16 px icon] 6 [title 13] 8 [2×2 dot] 8 [summary, ellipsized] [suffix]`,
//! one `kit::disclosure_row` each. A running row carries the 2.6 s sweep; a
//! failed one swaps its leading icon for a red state dot and its summary for
//! the failure's first line. Children (`parent_id`) are drawn recursively in
//! a 22 px-indented column behind a 0.5 px guide line — parallel calls get no
//! grouping chrome at all, exactly like dsh: they are consecutive siblings.
//!
//! The expanded body picks the first renderer that fits: a **diff block** for
//! `edit-file` / `write-file` (their arguments are literal, so the change is
//! the arguments), a **terminal block** for shells (the
//! `exit=… --- stdout --- …` framing `builtins::run_shell` writes), a **read
//! block** for line-numbered file reads, a **search block** for glob/grep,
//! otherwise the generic **IN/OUT card**.
//!
//! Bodies render `ToolChip::output` — the tool's own text as the model
//! received it — not `summary`, which for a large result is the expectation
//! summariser's paraphrase. When the two differ the paraphrase is shown under
//! the body, because that is what actually entered the context.

use egui::{Sense, Vec2};

use crate::app::{App, ToolChip};
use crate::ui::icons::Icon;
use crate::ui::kit::{self, DotState, Leading, Level, Weight};

/// Row title per variant, mirroring dsh's `tool-call-model.ts`.
pub fn title_of(skill: &str) -> &'static str {
    match skill {
        "glob" | "grep" => "Search",
        "read-file" => "Read",
        "run-cli" => "Bash",
        "run-pwsh" => "Pwsh",
        "write-file" => "Write",
        "edit-file" => "Edit",
        "skill-creator" | "model-eval" => "Code",
        "subagent" | "subagent-fork" | "ralph" | "agent-team" => "Delegate",
        "todo-write" => "To-dos",
        "ask-user" => "Ask",
        "exit-plan-mode" => "Plan review",
        _ => "Tool call",
    }
}

/// Draw every top-level chip of a turn, recursing into children.
pub fn draw(app: &mut App, ui: &mut egui::Ui, turn_idx: usize) {
    let chips = app.chat.turns[turn_idx].tool_chips.clone();
    if chips.is_empty() {
        return;
    }
    let roots: Vec<usize> = (0..chips.len())
        .filter(|&i| {
            chips[i].parent_id.is_none()
                || !chips.iter().any(|c| Some(c.id) == chips[i].parent_id)
        })
        .collect();
    for i in roots {
        draw_one(app, ui, turn_idx, &chips, i, 0);
    }
}

fn draw_one(
    app: &mut App,
    ui: &mut egui::Ui,
    turn_idx: usize,
    chips: &[ToolChip],
    idx: usize,
    depth: u8,
) {
    let chip = &chips[idx];
    let t = app.theme;
    let state = state_of(chip, app.chat.interrupt_requested);
    let leading = match state {
        ToolState::Error => Leading::Dot(DotState::Error),
        ToolState::Stopped => Leading::Dot(DotState::Warning),
        _ => Leading::Icon(Icon::for_skill(&chip.name)),
    };
    let summary = summary_of(chip, state);
    let title = title_of(&chip.name);
    let expanded = chip.expanded;
    // Collapsed edit/write rows carry `+A -R` in mono, like dsh's `.diffStat`.
    let stat = diff_stat(chip);
    let suffix = stat
        .as_ref()
        .map(|s| (s.as_str(), kit::col(t.alias.label[3])));

    let out = kit::disclosure_row(
        ui,
        leading,
        title,
        &summary,
        expanded,
        state == ToolState::Running,
        suffix,
    );
    let resp = out.response.on_hover_text(hover_of(chip));
    if out.clicked || resp.clicked() {
        if let Some(c) = app.chat.turns[turn_idx]
            .tool_chips
            .iter_mut()
            .find(|c| c.id == chip.id)
        {
            c.expanded = !expanded;
        }
    }

    if expanded {
        // The body is a sibling of the row, so clicks inside it never toggle.
        let chip = chip.clone();
        let mut action = None;
        indented(ui, 22.0, &t, |ui| {
            body(ui, &chip, state);
            action = row_actions(ui, &chip, &t);
        });
        match action {
            Some(RowAction::Inspect(seq)) => app.inspect_event(seq),
            Some(RowAction::Details) => {
                app.details_call = Some(chip.id);
                app.trajectory.selected = None;
                if app.layout.details_w <= 0.0 {
                    app.layout.details_w = sica_core::theme::tokens::DETAILS_DEFAULT;
                }
            }
            None => {}
        }
    }

    // Children: 22 px indent + a guide line, recursive.
    let kids: Vec<usize> = (0..chips.len())
        .filter(|&j| chips[j].parent_id == Some(chip.id))
        .collect();
    if !kids.is_empty() && depth < 4 {
        indented(ui, 22.0, &t, |ui| {
            for j in kids {
                draw_one(app, ui, turn_idx, chips, j, depth + 1);
            }
        });
    }
}

/// What the hover-revealed pills under an expanded body asked for.
enum RowAction {
    /// Open the Trajectory view focused on this call's durable event.
    Inspect(u64),
    /// Put this call's full payload in the details column.
    Details,
}

/// The pill row under an expanded body (§3.4): 0.5 px `border-l3`, r=999,
/// 11/16.
///
/// **Inspect** needs the call's durable `ToolCall` seq. A nested
/// `SkillContext::sub` call is a live event only — it never reaches the
/// session log — so on those the pill says why instead of jumping somewhere
/// arbitrary.
fn row_actions(
    ui: &mut egui::Ui,
    chip: &ToolChip,
    t: &sica_core::theme::Theme,
) -> Option<RowAction> {
    let mut action = None;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if chip.log_seq > 0 {
            if kit::pill(ui, "Inspect", false).clicked() {
                action = Some(RowAction::Inspect(chip.log_seq));
            }
        } else {
            kit::label(
                ui,
                kit::txt("Inspect", 11.0, kit::Weight::Regular, kit::col(t.alias.label[3])),
            )
            .on_hover_text("A nested call is a live event only — it is not in the session log.");
        }
        if kit::pill(ui, "Details", false).clicked() {
            action = Some(RowAction::Details);
        }
    });
    ui.add_space(2.0);
    action
}

/// A left-guided, indented column — the nesting chrome for child calls and
/// expanded bodies.
fn indented(
    ui: &mut egui::Ui,
    indent: f32,
    t: &sica_core::theme::Theme,
    body: impl FnOnce(&mut egui::Ui),
) {
    let resp = ui.horizontal(|ui| {
        ui.add_space(indent);
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 4.0).max(80.0));
            body(ui);
        });
    });
    let rect = resp.response.rect;
    ui.painter().vline(
        rect.min.x + indent - 8.0,
        rect.y_range(),
        egui::Stroke::new(sica_core::theme::tokens::HAIRLINE, Level::L2.color(t)),
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Error,
    Stopped,
}

fn state_of(chip: &ToolChip, stopping: bool) -> ToolState {
    if !chip.finished {
        if stopping {
            ToolState::Stopped
        } else {
            ToolState::Running
        }
    } else if chip.ok {
        ToolState::Ok
    } else {
        ToolState::Error
    }
}

/// Summary column: the failure's first line when the call failed, otherwise
/// the most identifying argument — a path, a pattern, a command — taken from
/// the rendered call text.
fn summary_of(chip: &ToolChip, state: ToolState) -> String {
    if state == ToolState::Error {
        let first = chip
            .summary
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        return kit::one_line(first, 140);
    }
    if let Some(arg) = first_quoted(&chip.args_preview) {
        return kit::one_line(&arg, 140);
    }
    let rest = chip
        .args_preview
        .strip_prefix(&chip.name)
        .unwrap_or(&chip.args_preview)
        .trim();
    if rest.is_empty() {
        // The `others` variant reads `{tool} · {base}` in dsh; with no args
        // to show, the tool name is the whole story.
        chip.name.clone()
    } else {
        kit::one_line(rest, 140)
    }
}

/// First `'…'` run of the rendered call, which is where the parser puts the
/// leading positional (path / pattern / command).
fn first_quoted(text: &str) -> Option<String> {
    let start = text.find('\'')? + 1;
    let end = text[start..].find('\'')? + start;
    let arg = text[start..end].trim();
    if arg.is_empty() {
        None
    } else {
        Some(arg.to_string())
    }
}

fn hover_of(chip: &ToolChip) -> String {
    let mut parts = vec![chip.args_preview.clone()];
    if !chip.expectation.is_empty() {
        parts.push(format!("expect: {}", chip.expectation));
    }
    parts.push(if chip.finished {
        let state = if chip.ok { "ok" } else { "failed" };
        if chip.duration_ms > 0 {
            format!("{state} · {}", fmt_ms(chip.duration_ms))
        } else {
            state.to_string()
        }
    } else {
        "running…".to_string()
    });
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

fn body(ui: &mut egui::Ui, chip: &ToolChip, state: ToolState) {
    ui.add_space(4.0);
    if state == ToolState::Running {
        kit::footnote(ui, "running…");
        ui.add_space(4.0);
        return;
    }
    // What the tool itself produced. Older sessions (and harness controls)
    // carry only the outcome, which is then the same text.
    let out = if chip.output.is_empty() { &chip.summary } else { &chip.output };
    let args = args_of(chip);
    if let Some(edit) = parse_edit(chip, &args) {
        diff_block(ui, chip, &edit);
    } else if let Some(term) = parse_shell(out) {
        terminal_block(ui, chip, &term);
    } else if chip.name == "read-file" && looks_line_numbered(out) {
        read_block(ui, out);
    } else if matches!(chip.name.as_str(), "glob" | "grep") && chip.ok {
        search_block(ui, chip, out);
    } else {
        kit::io_card(ui, &pretty_args(chip, &args), out, !chip.ok);
    }
    // The model read the paraphrase, not the text above — say so rather than
    // leaving the difference invisible.
    if !chip.output.is_empty() && chip.summary != chip.output {
        ui.add_space(4.0);
        kit::footnote(ui, "summarised for the model:");
        kit::footnote(ui, &kit::one_line(&chip.summary, 400));
    }
    if !chip.expectation.is_empty() {
        ui.add_space(4.0);
        kit::footnote(ui, &format!("expected: {}", chip.expectation));
    }
    if chip.duration_ms > 0 {
        kit::footnote(ui, &format!("took {}", fmt_ms(chip.duration_ms)));
    }
    ui.add_space(6.0);
}

fn fmt_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f32 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// The call's resolved arguments. Absent for a chip rebuilt from a log
/// written before `args_json` existed.
fn args_of(chip: &ToolChip) -> serde_json::Value {
    serde_json::from_str(&chip.args_json).unwrap_or(serde_json::Value::Null)
}

/// `key: value` lines for the IN half of the generic card — the arguments as
/// they were resolved, not the truncated one-line preview.
fn pretty_args(chip: &ToolChip, args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| {
                let text = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                format!("{k}: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => chip.args_preview.clone(),
    }
}

/// A literal edit: `edit-file` replaces `old` with `new` at one site, and
/// `write-file` writes `content` whole. Both are exact, so the change is the
/// arguments — no diff algorithm involved.
struct Edit {
    path: String,
    old:  String,
    new:  String,
}

fn parse_edit(chip: &ToolChip, args: &serde_json::Value) -> Option<Edit> {
    let obj = args.as_object()?;
    let get = |k: &str| obj.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    match chip.name.as_str() {
        "edit-file" => {
            let (old, new) = (get("old"), get("new"));
            if old.is_empty() && new.is_empty() {
                return None;
            }
            Some(Edit { path: get("path"), old, new })
        }
        "write-file" => {
            let content = get("content");
            if content.is_empty() {
                return None;
            }
            Some(Edit { path: get("path"), old: String::new(), new: content })
        }
        _ => None,
    }
}

/// `+A -R` for the collapsed row.
fn diff_stat(chip: &ToolChip) -> Option<String> {
    let edit = parse_edit(chip, &args_of(chip))?;
    let added = line_count(&edit.new);
    let removed = line_count(&edit.old);
    Some(format!("+{added} -{removed}"))
}

fn line_count(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.lines().count() }
}

/// Removed lines over added lines, tinted, capped at 224 px.
fn diff_block(ui: &mut egui::Ui, chip: &ToolChip, edit: &Edit) {
    let t = kit::theme(ui);
    egui::Frame::none()
        .fill(kit::col(t.alias.code_block))
        .stroke(egui::Stroke::new(
            sica_core::theme::tokens::HAIRLINE,
            Level::L1.color(&t),
        ))
        .rounding(egui::Rounding::same(sica_core::theme::tokens::RADIUS_CARD))
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !edit.path.is_empty() {
                kit::label(ui, kit::mono(&edit.path, 11.0, kit::col(t.alias.label[2])));
                ui.add_space(4.0);
            }
            egui::ScrollArea::vertical()
                .id_source(ui.id().with(("diff", chip.id)))
                .max_height(224.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for line in edit.old.lines() {
                        ui.add(
                            egui::Label::new(kit::mono(
                                format!("- {line}"),
                                11.0,
                                kit::col(t.alias.error),
                            ))
                            .wrap(),
                        );
                    }
                    for line in edit.new.lines() {
                        ui.add(
                            egui::Label::new(kit::mono(
                                format!("+ {line}"),
                                11.0,
                                kit::col(t.alias.success),
                            ))
                            .wrap(),
                        );
                    }
                });
        });
}

/// `{n} matches` over the first 8 result lines (§3.4).
fn search_block(ui: &mut egui::Ui, chip: &ToolChip, text: &str) {
    const CAP: usize = 8;
    let t = kit::theme(ui);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let id = ui.id().with(("search_all", chip.id));
    let all: bool = ui.ctx().data(|d| d.get_temp(id).unwrap_or(false));
    let shown = if all { lines.len() } else { lines.len().min(CAP) };
    kit::code_block(ui, "matches", &lines[..shown].join("\n"));
    if lines.len() > CAP {
        let label = if all {
            "Show less".to_string()
        } else {
            format!("Showing {shown} of {} lines", lines.len())
        };
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::click());
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            kit::font(11.0, Weight::Regular),
            kit::col(t.alias.label[2]),
        );
        if resp.clicked() {
            ui.ctx().data_mut(|d| d.insert_temp(id, !all));
        }
    }
}

struct Shell {
    exit: i32,
    stdout: String,
    stderr: String,
}

/// `exit={code}\n--- stdout ---\n…\n--- stderr ---\n…` — the shape
/// `agents::builtins::run_shell` writes.
fn parse_shell(text: &str) -> Option<Shell> {
    let rest = text.strip_prefix("exit=")?;
    let (code, rest) = rest.split_once('\n')?;
    let exit = code.trim().parse::<i32>().ok()?;
    let rest = rest.strip_prefix("--- stdout ---\n")?;
    let (stdout, stderr) = match rest.split_once("--- stderr ---\n") {
        Some((o, e)) => (o.trim_end().to_string(), e.trim_end().to_string()),
        None => (rest.trim_end().to_string(), String::new()),
    };
    Some(Shell { exit, stdout, stderr })
}

/// Prompt row + output, capped at 224 px like dsh's terminal block.
fn terminal_block(ui: &mut egui::Ui, chip: &ToolChip, sh: &Shell) {
    let t = kit::theme(ui);
    let command = args_of(chip)
        .get("command")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| first_quoted(&chip.args_preview))
        .unwrap_or_else(|| chip.name.clone());
    egui::Frame::none()
        .fill(kit::col(t.alias.code_block))
        .stroke(egui::Stroke::new(
            sica_core::theme::tokens::HAIRLINE,
            Level::L1.color(&t),
        ))
        .rounding(egui::Rounding::same(sica_core::theme::tokens::RADIUS_CARD))
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                kit::state_dot(
                    ui,
                    if sh.exit == 0 { DotState::Done } else { DotState::Error },
                    10.0,
                );
                ui.add_space(6.0);
                kit::label(ui, kit::mono("$", 11.0, kit::col(t.alias.label[3])));
                ui.add(
                    egui::Label::new(kit::mono(&command, 11.0, kit::col(t.alias.label[0])))
                        .wrap(),
                );
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_source(ui.id().with(("term", chip.id)))
                .max_height(224.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if sh.stdout.is_empty() && sh.stderr.is_empty() {
                        kit::label(ui, kit::mono("No output", 11.0, kit::col(t.alias.label[3])));
                    }
                    if !sh.stdout.is_empty() {
                        ui.add(
                            egui::Label::new(kit::mono(&sh.stdout, 11.0, kit::col(t.alias.label[0])))
                                .wrap(),
                        );
                    }
                    if !sh.stderr.is_empty() {
                        ui.add(
                            egui::Label::new(kit::mono(&sh.stderr, 11.0, kit::col(t.alias.error)))
                                .wrap(),
                        );
                    }
                });
            ui.add_space(4.0);
            let (label, color) = if sh.exit == 0 {
                ("exit code 0".to_string(), kit::col(t.alias.label[2]))
            } else {
                (format!("exit code {}", sh.exit), kit::col(t.alias.error))
            };
            kit::label(ui, kit::txt(label, 11.0, Weight::Regular, color));
        });
}

fn looks_line_numbered(text: &str) -> bool {
    text.lines()
        .take(3)
        .any(|l| l.split_once('\t').map(|(n, _)| n.trim().parse::<u32>().is_ok()).unwrap_or(false))
}

/// Line-numbered read, capped at 8 lines with `Showing {shown} of {total}`.
fn read_block(ui: &mut egui::Ui, text: &str) {
    const CAP: usize = 8;
    let t = kit::theme(ui);
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(CAP);
    let id = ui.id().with(("read_all", text.len()));
    let all: bool = ui.ctx().data(|d| d.get_temp(id).unwrap_or(false));
    let body = if all || lines.len() <= CAP {
        text.to_string()
    } else {
        lines[..shown].join("\n")
    };
    kit::code_block(ui, "file", &body);
    if lines.len() > CAP {
        let label = if all {
            "Show less".to_string()
        } else {
            format!("Showing {shown} of {} lines", lines.len())
        };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::click());
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            kit::font(11.0, Weight::Regular),
            kit::col(t.alias.label[2]),
        );
        if resp.clicked() {
            ui.ctx().data_mut(|d| d.insert_temp(id, !all));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shell_result_framing() {
        let sh = parse_shell("exit=1\n--- stdout ---\nhello\n--- stderr ---\nboom\n").unwrap();
        assert_eq!(sh.exit, 1);
        assert_eq!(sh.stdout, "hello");
        assert_eq!(sh.stderr, "boom");
        // A summarised result (the >2 KB path) is not shell-shaped and must
        // fall through to the IN/OUT card rather than render as a terminal.
        assert!(parse_shell("The build succeeded.").is_none());
    }

    #[test]
    fn summary_prefers_the_leading_positional() {
        assert_eq!(
            first_quoted("read-file 'crates/frontend/src/app.rs' > the App struct"),
            Some("crates/frontend/src/app.rs".to_string())
        );
        assert_eq!(first_quoted("job-list"), None);
    }

    #[test]
    fn titles_follow_the_variant_map() {
        assert_eq!(title_of("run-pwsh"), "Pwsh");
        assert_eq!(title_of("grep"), "Search");
        assert_eq!(title_of("some-markdown-skill"), "Tool call");
    }
}
