//! `/command` and `@path` decoration inside a user message (§3.1).
//!
//! dsh renders these as inline chips in a contenteditable; `TextEdit` cannot
//! embed widgets, so — as the guide's §13 concedes — they are painted as
//! coloured runs, which is what dsh itself does once the message is sent.

use egui::{text::LayoutJob, Color32, FontId, TextFormat};

/// Lay out `text` with `/name` (leading token only, as `agents::invoke`
/// resolves it) and `@path` runs in `accent`.
pub fn job(text: &str, base: Color32, accent: Color32, font: FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let plain = TextFormat {
        font_id: font.clone(),
        color: base,
        ..Default::default()
    };
    let marked = TextFormat {
        font_id: font,
        color: accent,
        ..Default::default()
    };
    let mut first_token = true;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            job.append("\n", 0.0, plain.clone());
        }
        for (j, word) in line.split(' ').enumerate() {
            if j > 0 {
                job.append(" ", 0.0, plain.clone());
            }
            let is_command = first_token && word.starts_with('/') && word.len() > 1;
            let is_path = word.starts_with('@') && word.len() > 1;
            job.append(
                word,
                0.0,
                if is_command || is_path {
                    marked.clone()
                } else {
                    plain.clone()
                },
            );
            if !word.is_empty() {
                first_token = false;
            }
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(text: &str) -> Vec<(String, Color32)> {
        let j = job(
            text,
            Color32::BLACK,
            Color32::BLUE,
            FontId::proportional(14.0),
        );
        j.sections
            .iter()
            .map(|s| (j.text[s.byte_range.clone()].to_string(), s.format.color))
            .collect()
    }

    #[test]
    fn marks_a_leading_command_and_any_at_path() {
        let runs = colors("/plan add @src/main.rs now");
        assert_eq!(runs[0], ("/plan".into(), Color32::BLUE));
        assert!(runs
            .iter()
            .any(|(t, c)| t == "@src/main.rs" && *c == Color32::BLUE));
        assert!(runs.iter().any(|(t, c)| t == "now" && *c == Color32::BLACK));
    }

    #[test]
    fn a_slash_mid_message_is_not_a_command() {
        let runs = colors("use a/b and /nope");
        assert!(runs
            .iter()
            .all(|(t, c)| t != "/nope" || *c == Color32::BLACK));
    }
}
