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

/// Resolve the image targets in a message against the session's working
/// directory, and refuse the ones that reach outside it.
///
/// `egui_commonmark` turns a bare `![x](a.png)` into `file://a.png`, which
/// resolves against **the process's** current directory — wherever the app
/// happened to be launched from. That is never what the model meant: it
/// writes paths relative to the folder it is working in (§4.3). So local
/// targets are rewritten to an absolute `file://` URI under `cwd`.
///
/// A target that escapes `cwd` is **not** rewritten into something that
/// loads. The model's prose is not a capability: an assistant that writes
/// `![](../../../secrets.png)` must not thereby get the app to open it. Such
/// a target is left visible as inline code, so the reader can see exactly
/// what was asked for rather than watching an image silently not appear.
/// `http://` and `https://` are left alone — a remote image is the model
/// quoting the web, and the loader treats it as remote either way.
pub fn rewrite_image_uris(text: &str, cwd: &std::path::Path) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if fence_marker(trimmed).is_some() {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        out.push_str(&rewrite_line(line, cwd));
    }
    out
}

fn rewrite_line(line: &str, cwd: &std::path::Path) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find("![") {
        let (before, from) = rest.split_at(open);
        out.push_str(before);
        let Some(alt_end) = from.find("](") else {
            out.push_str(from);
            return out;
        };
        let Some(close) = from[alt_end..].find(')').map(|k| k + alt_end) else {
            out.push_str(from);
            return out;
        };
        let alt = &from[2..alt_end];
        let target = from[alt_end + 2..close].trim();
        out.push_str(&rewritten(alt, target, cwd));
        rest = &from[close + 1..];
    }
    out.push_str(rest);
    out
}

fn rewritten(alt: &str, target: &str, cwd: &std::path::Path) -> String {
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return format!("![{alt}]({target})");
    }
    let raw = target.strip_prefix("file://").unwrap_or(target);
    // Windows `file:///C:/x` keeps a leading slash the path parser does not
    // want.
    let raw = raw.strip_prefix('/').filter(|r| r.chars().nth(1) == Some(':')).unwrap_or(raw);
    match resolve_under(cwd, raw) {
        Some(path) => format!("![{alt}](file://{})", path.display().to_string().replace('\\', "/")),
        None => {
            // Refused, and said so: the alt text plus the path as written.
            if alt.is_empty() {
                format!("`{target}`")
            } else {
                format!("{alt} (`{target}`)")
            }
        }
    }
}

/// Join `rel` onto `root` without letting it climb out. Lexical, like the
/// backend's own `resolve` (harness §6.6): a path is refused for what it
/// says, not for what the filesystem would make of it.
fn resolve_under(root: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path};
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return candidate.starts_with(root).then(|| candidate.to_path_buf());
    }
    let mut depth: i32 = 0;
    for c in candidate.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            Component::CurDir => {}
            // A rooted-but-driveless `\foo` or a fresh prefix is not a
            // relative path at all.
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(root.join(candidate))
}

/// Turn inline code that names a real file under `cwd` into a link.
///
/// The model is asked to name what it produced as inline code with the
/// exact path (harness §5.1); this is the half that makes that useful. The
/// same fence as images applies, and for the same reason: prose is not a
/// capability, so only a path that stays inside the session's folder — and
/// that actually exists — becomes something the app will open. Everything
/// else stays exactly as the model wrote it.
pub fn linkify_file_paths(text: &str, cwd: &std::path::Path) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if fence_marker(trimmed).is_some() {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        out.push_str(&linkify_line(line, cwd));
    }
    out
}

fn linkify_line(line: &str, cwd: &std::path::Path) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let (before, from) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = from[1..].find('`').map(|k| k + 1) else {
            out.push_str(from);
            return out;
        };
        let body = &from[1..close];
        // An image or link target that already has the code in it must not
        // be wrapped again.
        let already_linked = before.ends_with('[') || before.ends_with('(');
        match (already_linked, file_link(body, cwd)) {
            (false, Some(uri)) => out.push_str(&format!("[`{body}`]({uri})")),
            _ => out.push_str(&from[..=close]),
        }
        rest = &from[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The `file://` URI for an inline-code span, when it names a file that is
/// really there and really inside `cwd`.
fn file_link(body: &str, cwd: &std::path::Path) -> Option<String> {
    let candidate = body.trim();
    // A command line, a symbol, a sentence — none of these are paths, and
    // guessing wrong turns ordinary prose into a wall of links.
    if candidate.is_empty()
        || candidate.contains(char::is_whitespace)
        || !candidate.contains('.')
        || candidate.starts_with('-')
    {
        return None;
    }
    let path = resolve_under(cwd, candidate)?;
    path.is_file()
        .then(|| format!("file://{}", path.display().to_string().replace('\\', "/")))
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

    /// `/proj` is not an absolute path on Windows — it is rooted but
    /// driveless, which is one of the shapes §3.9 refuses — so the fixture
    /// is a real absolute path on whichever platform is running.
    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn uri(path: &std::path::Path) -> String {
        format!("file://{}", path.display().to_string().replace('\\', "/"))
    }

    #[test]
    fn a_local_image_resolves_against_the_session_folder() {
        let cwd = root();
        let out = rewrite_image_uris("see ![chart](out/chart.png) here", &cwd);
        assert_eq!(out, format!("see ![chart]({}) here", uri(&cwd.join("out/chart.png"))));
        // Already absolute and inside: kept.
        let inside = cwd.join("a.png");
        let abs = rewrite_image_uris(&format!("![x]({})", inside.display()), &cwd);
        assert_eq!(abs, format!("![x]({})", uri(&inside)));
        // Remote is left exactly as written.
        let remote = "![logo](https://example.com/a.png)";
        assert_eq!(rewrite_image_uris(remote, &cwd), remote);
    }

    /// Prose is not a capability: a path that climbs out of the session's
    /// folder must not become something the app will open.
    #[test]
    fn an_image_reaching_outside_the_folder_is_refused_visibly() {
        let cwd = root();
        let out = rewrite_image_uris("![secret](../../etc/shadow.png)", &cwd);
        assert_eq!(out, "secret (`../../etc/shadow.png`)");
        assert!(!out.contains("file://"), "a refused target must not load");
        // Elsewhere on disk, spelled absolutely.
        let elsewhere = cwd.parent().unwrap().join("elsewhere.png");
        let abs = rewrite_image_uris(&format!("![x]({})", elsewhere.display()), &cwd);
        assert!(!abs.contains("file://"), "{abs}");
        // A rooted-but-driveless path resolves against the process drive on
        // Windows, so it is refused rather than rebased (§3.9).
        assert!(!rewrite_image_uris("![x](/etc/shadow.png)", &cwd).contains("file://"));
        // No alt text still shows the path rather than vanishing.
        assert_eq!(rewrite_image_uris("![](../x.png)", &cwd), "`../x.png`");
    }

    #[test]
    fn image_syntax_inside_a_fence_is_left_alone() {
        let cwd = root();
        let cwd = cwd.as_path();
        let text = "```md\n![x](a.png)\n```\n![y](b.png)";
        let out = rewrite_image_uris(text, cwd);
        assert!(out.contains("```md\n![x](a.png)"), "{out}");
        assert!(out.contains(&format!("![y]({})", uri(&cwd.join("b.png")))), "{out}");
    }

    #[test]
    fn only_a_real_file_inside_the_folder_becomes_a_link() {
        let cwd = root();
        // This very file, named the way the model is asked to name it.
        let rel = "src/ui/chat/md_blocks.rs";
        assert!(cwd.join(rel).is_file(), "fixture moved");
        let out = linkify_file_paths(&format!("wrote `{rel}` today"), &cwd);
        assert_eq!(out, format!("wrote [`{rel}`]({}) today", uri(&cwd.join(rel))));

        // A file that does not exist stays prose: a link that opens nothing
        // is worse than no link.
        let missing = "src/nope.rs";
        assert_eq!(
            linkify_file_paths(&format!("`{missing}`"), &cwd),
            format!("`{missing}`")
        );
        // Outside the folder, even if it exists.
        assert_eq!(linkify_file_paths("`../Cargo.toml`", &cwd), "`../Cargo.toml`");
        // Not a path at all.
        for code in ["cargo test", "String", "-v", "x.y"] {
            let src = format!("`{code}`");
            assert_eq!(linkify_file_paths(&src, &cwd), src, "{code} was linkified");
        }
    }

    #[test]
    fn linkifying_leaves_fences_and_existing_links_alone() {
        let cwd = root();
        let rel = "Cargo.toml";
        assert!(cwd.join(rel).is_file());
        let fenced = format!("```\n`{rel}`\n```");
        assert_eq!(linkify_file_paths(&fenced, &cwd), fenced);
        // Already a link target: not wrapped a second time.
        let linked = format!("[`{rel}`](file://x)");
        assert_eq!(linkify_file_paths(&linked, &cwd), linked);
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
