//! Markdown extras for the assistant body (§3.8).
//!
//! `egui_commonmark` renders prose well and knows nothing about two things
//! the model writes constantly: **display math** and **wide tables**. Both
//! are handled the same way — by splitting the body into segments *before*
//! the viewer sees it, so each segment can be rendered by whatever suits it.
//!
//! **Math is a deliberate non-port.** There is no KaTeX for egui, and a
//! half-typeset formula is worse than none: CommonMark's `_` and `*` rules
//! mangle TeX source into italics and dropped characters, so `x_1` loses its
//! subscript and the reader cannot even recover what was written. Rendering
//! the source verbatim in a code block keeps it legible and copyable, which
//! is the honest fallback until something can typeset it.
//!
//! **A wide table scrolls inside itself.** Left to the viewer it widens the
//! whole conversation column, which moves every other message on the screen.
//!
//! Everything here is a pure function over the source string: the renderer
//! matches on the result, and the rules live where they can be tested.

/// One piece of an assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Ordinary markdown — headings, prose, lists, fenced code.
    Prose(String),
    /// Display math (`$$…$$` or `\[…\]`), TeX source without its delimiters.
    Math(String),
    /// A GFM table, rendered inside its own horizontal scroll.
    Table(String),
}

/// Split an assistant message into renderable blocks.
///
/// Fenced code is opaque: a `$$` or a `|` inside a fence is code the model
/// is *showing*, not markup it is using, and splitting there would tear the
/// fence in half.
pub fn split(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<Block> = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let mut i = 0;

    // Flush whatever prose has accumulated, dropping an all-blank run.
    fn flush(prose: &mut Vec<&str>, out: &mut Vec<Block>) {
        if prose.iter().any(|l| !l.trim().is_empty()) {
            out.push(Block::Prose(prose.join("\n")));
        }
        prose.clear();
    }

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        // A fence runs to its closing marker, or to the end of the message
        // (a stream cut mid-block is normal while a turn is live).
        if let Some(fence) = fence_marker(trimmed) {
            prose.push(line);
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                prose.push(l);
                i += 1;
                if l.trim_start().starts_with(fence) {
                    break;
                }
            }
            continue;
        }

        if let Some((body, next)) = display_math(&lines, i) {
            flush(&mut prose, &mut out);
            out.push(Block::Math(body));
            i = next;
            continue;
        }

        if let Some((body, next)) = table(&lines, i) {
            flush(&mut prose, &mut out);
            out.push(Block::Table(body));
            i = next;
            continue;
        }

        prose.push(line);
        i += 1;
    }
    flush(&mut prose, &mut out);
    out
}

/// The fence marker a line opens, if it opens one.
fn fence_marker(trimmed: &str) -> Option<&'static str> {
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// A display-math block starting at `i`: `$$ … $$` or `\[ … \]`, either on
/// one line or spanning several. Returns the TeX source and the line after.
fn display_math(lines: &[&str], i: usize) -> Option<(String, usize)> {
    let first = lines[i].trim();
    let (open, close) = if first.starts_with("$$") {
        ("$$", "$$")
    } else if first.starts_with("\\[") {
        ("\\[", "\\]")
    } else {
        return None;
    };

    // One-liner: `$$ x = 1 $$`.
    let rest = &first[open.len()..];
    if let Some(end) = rest.find(close) {
        let body = rest[..end].trim().to_string();
        // `$$$$` with nothing in it is not math, it is punctuation.
        return (!body.is_empty()).then_some((body, i + 1));
    }

    let mut body: Vec<&str> = Vec::new();
    if !rest.trim().is_empty() {
        body.push(rest.trim());
    }
    let mut j = i + 1;
    while j < lines.len() {
        let line = lines[j];
        if let Some(end) = line.find(close) {
            let head = line[..end].trim();
            if !head.is_empty() {
                body.push(head);
            }
            return Some((body.join("\n"), j + 1));
        }
        body.push(line);
        j += 1;
    }
    // Unclosed: the turn is probably still streaming. Leave it to prose so
    // the text keeps appearing rather than vanishing into a pending block.
    None
}

/// A GFM table starting at `i`: a header row of cells, then a delimiter row
/// (`---|:--:|---`), then rows until a line that is not one.
fn table(lines: &[&str], i: usize) -> Option<(String, usize)> {
    let header = lines[i].trim();
    if !header.contains('|') {
        return None;
    }
    let delim = lines.get(i + 1)?.trim();
    if !is_delimiter_row(delim) {
        return None;
    }
    let mut j = i + 2;
    while j < lines.len() {
        let l = lines[j].trim();
        if l.is_empty() || !l.contains('|') {
            break;
        }
        j += 1;
    }
    Some((lines[i..j].join("\n"), j))
}

/// `|---|:---:|` — the row that makes the line above it a table header.
fn is_delimiter_row(line: &str) -> bool {
    if !line.contains('-') || !line.contains('|') {
        return false;
    }
    line.chars()
        .all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t'))
}

/// Rewrite inline math as inline code, for the same reason display math
/// becomes a code block: the parser would otherwise eat the TeX.
///
/// Only `$…$` and `\(…\)` outside code spans, and only when the opening
/// delimiter is followed by a non-space — `$5 and $6` is money, not math,
/// and dsh's renderer draws the line in the same place.
pub fn inline_math_to_code(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_code = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '`' {
            in_code = !in_code;
            out.push(c);
            i += 1;
            continue;
        }
        if in_code {
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\\' && bytes.get(i + 1) == Some(&'(') {
            if let Some(end) = find_seq(&bytes, i + 2, '\\', ')') {
                let body: String = bytes[i + 2..end].iter().collect();
                push_code(&mut out, body.trim());
                i = end + 2;
                continue;
            }
        }
        if c == '$' && bytes.get(i + 1).is_some_and(|n| !n.is_whitespace() && *n != '$') {
            if let Some(end) = bytes[i + 1..].iter().position(|c| *c == '$').map(|p| p + i + 1) {
                let body: String = bytes[i + 1..end].iter().collect();
                // A closing `$` right after a space is the other end of a
                // price range, not a formula.
                if !body.ends_with(char::is_whitespace) && !body.contains('`') {
                    push_code(&mut out, body.trim());
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Wrap `body` in backticks, using a longer fence when it holds one.
fn push_code(out: &mut String, body: &str) {
    if body.is_empty() {
        return;
    }
    out.push('`');
    out.push_str(body);
    out.push('`');
}

fn find_seq(chars: &[char], from: usize, a: char, b: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1))
        .find(|&k| chars[k] == a && chars[k + 1] == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose(s: &str) -> Block {
        Block::Prose(s.to_string())
    }

    #[test]
    fn a_message_without_extras_is_one_block() {
        let text = "# Title\n\nSome prose with `code` in it.\n\n- a\n- b";
        assert_eq!(split(text), vec![prose(text)]);
    }

    #[test]
    fn display_math_becomes_its_own_block_both_ways() {
        let one = split("before\n\n$$ E = mc^2 $$\n\nafter");
        assert_eq!(
            one,
            vec![
                prose("before\n"),
                Block::Math("E = mc^2".into()),
                prose("\nafter"),
            ]
        );
        let many = split("$$\n\\sum_{i=1}^n i\n$$");
        assert_eq!(many, vec![Block::Math("\\sum_{i=1}^n i".into())]);
        let bracket = split("\\[\nx = 1\n\\]");
        assert_eq!(bracket, vec![Block::Math("x = 1".into())]);
    }

    /// A fence is opaque: what is inside it is what the model is showing.
    #[test]
    fn nothing_inside_a_fence_is_split() {
        let text = "```tex\n$$ x $$\n| a | b |\n|---|---|\n```";
        assert_eq!(split(text), vec![prose(text)]);
        // A fence left open by a stream cut still keeps its contents.
        let cut = "```\n$$ x $$";
        assert_eq!(split(cut), vec![prose(cut)]);
    }

    #[test]
    fn a_table_is_split_out_whole() {
        let text = "intro\n\n| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |\n\nafter";
        assert_eq!(
            split(text),
            vec![
                prose("intro\n"),
                Block::Table("| a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |".into()),
                prose("\nafter"),
            ]
        );
        // A pipe without a delimiter row underneath is prose, not a table.
        let pipes = "a | b\nc | d";
        assert_eq!(split(pipes), vec![prose(pipes)]);
    }

    /// The stream arrives a token at a time; every prefix of a message has
    /// to render as *something*, and never lose text.
    #[test]
    fn every_prefix_of_a_message_keeps_all_of_its_text() {
        let full = "Result:\n\n$$ a + b $$\n\n| x | y |\n|---|---|\n| 1 | 2 |\n\ndone";
        for n in 1..=full.len() {
            if !full.is_char_boundary(n) {
                continue;
            }
            let blocks = split(&full[..n]);
            let seen: String = blocks
                .iter()
                .map(|b| match b {
                    Block::Prose(s) | Block::Table(s) => s.clone(),
                    Block::Math(s) => s.clone(),
                })
                .collect::<Vec<_>>()
                .join("");
            for word in ["Result", "done"] {
                if full[..n].contains(word) {
                    assert!(seen.contains(word), "prefix {n} lost {word}: {blocks:?}");
                }
            }
        }
    }

    #[test]
    fn inline_math_becomes_inline_code_and_money_does_not() {
        assert_eq!(inline_math_to_code("let $x_1$ be"), "let `x_1` be");
        assert_eq!(inline_math_to_code("also \\(y^2\\) here"), "also `y^2` here");
        // Prices: an opening delimiter followed by a space, or a closing one
        // preceded by a space, is not a formula.
        assert_eq!(inline_math_to_code("costs $5 and $6 total"), "costs $5 and $6 total");
        assert_eq!(inline_math_to_code("$ x $"), "$ x $");
        // Code spans are left alone — `$x$` inside them is what was written.
        assert_eq!(inline_math_to_code("`$x$` stays"), "`$x$` stays");
        // Nothing to close: the text survives unchanged.
        assert_eq!(inline_math_to_code("a lone $ sign"), "a lone $ sign");
    }
}
