//! Tokenising a session log so two runs of it can be diffed (guide §14.1).
//!
//! A replayed session is byte-identical to its recording in everything that
//! matters and different in everything that does not: the clock moved, the
//! session got a new id, the working directory is a temp folder. Comparing
//! raw JSONL would therefore always fail, and comparing "roughly" would
//! never fail for the right reason.
//!
//! So: replace the volatile identities with **typed tokens** —
//! `{{session}}`, `{{ts}}`, `{{cwd}}`, `"{{system}}"`, `"{{tools}}"` — and
//! leave everything else exactly as written. dsh's rule, kept verbatim:
//! *never redact arbitrary user or tool text merely because it resembles an
//! identifier.* Only the named fields below are touched, so a user message
//! that happens to contain a number, a path, or a timestamp survives into
//! the diff and a change to it fails the scenario, which is the point.
//!
//! The system prompt and the tools array are replaced by tokens rather than
//! kept: they are kilobytes each and change whenever a skill's description
//! is edited, which would make every scenario fail for a reason no scenario
//! is about. [`Sidecar`] carries them out separately so a diff stays
//! readable and the bodies are still inspectable.

use serde_json::{Map, Value};

/// The system prompt and tools array lifted out of the log, keyed by the
/// envelope fingerprint they belonged to. Written next to a recording as
/// `prompts.json` so a reviewer can still read what the model was told.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// `(fingerprint, system, tools)`, in the order the envelopes appeared.
    pub envelopes: Vec<(u64, String, String)>,
}

/// Normalise one recorded log.
///
/// `cwd` is the working directory the run used; every occurrence of it in
/// any string value becomes `{{cwd}}`. Returns the tokenised JSONL and the
/// sidecar of prompt bodies.
pub fn normalize(jsonl: &str, cwd: &str) -> (String, Sidecar) {
    normalize_paths(jsonl, &[(cwd, "{{cwd}}")])
}

/// [`normalize`] with more than one path to tokenise.
///
/// A replay run has two: the working directory the agent acts on, and the
/// scratch root its sessions and spill files live under. Both carry the
/// run's pid, and both appear in the log. Longest first, so a root that is
/// a prefix of the working directory does not swallow it.
pub fn normalize_paths(jsonl: &str, paths: &[(&str, &str)]) -> (String, Sidecar) {
    let mut sidecar = Sidecar::default();
    let mut out = String::new();
    let ordered = order_paths(paths);
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(mut v) = serde_json::from_str::<Value>(line) else {
            // A torn tail is part of what a recording may hold. Keep it as
            // written so the diff says so rather than silently dropping it.
            out.push_str(line);
            out.push('\n');
            continue;
        };
        if let Some(obj) = v.as_object_mut() {
            normalize_event(obj, &mut sidecar);
        }
        replace_paths(&mut v, &ordered);
        out.push_str(&serde_json::to_string(&v).unwrap_or_else(|_| line.to_string()));
        out.push('\n');
    }
    (out, sidecar)
}

/// Replace `cwd` with `{{cwd}}` throughout, and nothing else.
///
/// What `--bless` writes: a recording has to stay *deserialisable* (the
/// replay script is built from it, and `ts` is an `i64`), so it cannot be a
/// fully tokenised log — but the path the recording ran under is the one
/// thing that would otherwise differ between the machine that recorded it
/// and every machine that replays it.
pub fn mask_cwd(jsonl: &str, cwd: &str) -> String {
    mask_paths(jsonl, &[(cwd, "{{cwd}}")])
}

/// [`mask_cwd`] with more than one path to tokenise.
pub fn mask_paths(jsonl: &str, paths: &[(&str, &str)]) -> String {
    let ordered = order_paths(paths);
    if ordered.is_empty() {
        return jsonl.to_string();
    }
    let mut out = String::with_capacity(jsonl.len());
    for line in jsonl.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(mut v) => {
                replace_paths(&mut v, &ordered);
                out.push_str(&serde_json::to_string(&v).unwrap_or_else(|_| line.to_string()));
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Field-by-field, by name. Nothing here is a heuristic over values.
fn normalize_event(obj: &mut Map<String, Value>, sidecar: &mut Sidecar) {
    // Every event carries a wall-clock stamp. It is never part of what a
    // scenario asserts.
    if obj.contains_key("ts") {
        obj.insert("ts".into(), Value::String("{{ts}}".into()));
    }
    let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
    match kind.as_str() {
        "session_created" => {
            // The id is assigned by whatever the store's high-water mark
            // happened to be, and the default title is derived from it.
            obj.insert("id".into(), Value::String("{{session}}".into()));
            obj.insert("created_at".into(), Value::String("{{ts}}".into()));
            if let Some(Value::String(t)) = obj.get("title") {
                if is_default_title(t) {
                    obj.insert("title".into(), Value::String("{{session-title}}".into()));
                }
            }
        }
        "request_envelope" => {
            let fingerprint = obj.get("fingerprint").and_then(|v| v.as_u64()).unwrap_or(0);
            let system = take_string(obj, "system");
            let tools = take_string(obj, "tools");
            sidecar.envelopes.push((fingerprint, system, tools));
            // The fingerprint hashes the bodies, so it moves whenever a
            // skill description is edited — the same reason the bodies go
            // to the sidecar.
            obj.insert("fingerprint".into(), Value::String("{{fingerprint}}".into()));
            obj.insert("system".into(), Value::String("{{system}}".into()));
            if obj.contains_key("tools") {
                obj.insert("tools".into(), Value::String("{{tools}}".into()));
            }
        }
        "llm_retry" => {
            // The backoff carries deliberate jitter, so no two runs agree
            // on it. That a retry happened, at which attempt and for what
            // reason, is what a scenario asserts.
            obj.insert("delay_ms".into(), Value::String("{{delay}}".into()));
        }
        "context_injected" => {
            // The runtime-context snapshot is time, cwd, os and model. Only
            // the time line is volatile; `replace_paths` handles the cwd,
            // and the model is part of what the scenario ran against.
            if let Some(Value::String(c)) = obj.get("content") {
                let replaced = mask_times(c);
                obj.insert("content".into(), Value::String(replaced));
            }
        }
        _ => {}
    }
}

fn take_string(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// `Session 12` — the placeholder title a session gets before anything is
/// in it. A title a human or the titler wrote is content and stays.
fn is_default_title(t: &str) -> bool {
    t.strip_prefix("Session ")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Replace the run's working directory wherever it appears in a string,
/// in both separator styles — the same path reaches the log as
/// `C:\work\x` from one producer and `C:/work/x` from another.
/// The paths to tokenise, longest first and with the separator variants
/// each one can reach the log in: as written, forward-slashed, and
/// backslash-escaped (a path inside a tool's `args_json` is embedded JSON).
fn order_paths(paths: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut sorted: Vec<&(&str, &str)> = paths.iter().filter(|(p, _)| !p.is_empty()).collect();
    sorted.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
    for (path, token) in sorted {
        for variant in [
            path.to_string(),
            path.replace('\\', "/"),
            path.replace('\\', "\\\\"),
        ] {
            if !out.iter().any(|(p, _)| p == &variant) {
                out.push((variant, token.to_string()));
            }
        }
    }
    out
}

fn replace_paths(v: &mut Value, paths: &[(String, String)]) {
    match v {
        Value::String(s) => {
            *s = mask_spill(s);
            for (path, token) in paths {
                if s.contains(path.as_str()) {
                    *s = s.replace(path.as_str(), token);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| replace_paths(i, paths)),
        Value::Object(map) => map.values_mut().for_each(|i| replace_paths(i, paths)),
        _ => {}
    }
}

/// Replace a spill filename with `{{spill}}`.
///
/// A spill path is `…/spill/<label>/<skill>-<unix-secs>-<id>.txt`: the
/// directory is covered by the cwd replacement, but the seconds and the id
/// in the *filename* move on every run. Matched structurally — a `spill`
/// path **segment**, one label segment, then a basename ending in
/// `-<digits>-<digits>.txt` — so `notes/spill-2024-01.txt`, which is
/// somebody's file rather than ours, is left alone.
fn mask_spill(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let Some(at) = find_spill_segment(rest) else { break };
        let (before, tail) = rest.split_at(at);
        match spill_span(tail) {
            Some(len) => {
                out.push_str(before);
                out.push_str("{{spill}}");
                rest = &tail[len..];
            }
            None => {
                // Not one of ours: copy past this occurrence and keep going.
                let step = at + "spill".len();
                out.push_str(&rest[..step]);
                rest = &rest[step..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Offset of a `spill` **segment** — followed by `/` or `\`, and either at
/// the start or preceded by a separator.
fn find_spill_segment(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(rel) = s[from..].find("spill") {
        let at = from + rel;
        let after = at + "spill".len();
        let sep_after = matches!(b.get(after), Some(b'/') | Some(b'\\'));
        let sep_before = at == 0 || matches!(b.get(at - 1), Some(b'/') | Some(b'\\'));
        if sep_after && sep_before {
            return Some(at);
        }
        from = after;
    }
    None
}

/// Length of the spill path starting at `s`, or `None` when the shape does
/// not match.
fn spill_span(s: &str) -> Option<usize> {
    let end = s.find(".txt")? + ".txt".len();
    let candidate = &s[..end];
    // No whitespace or quoting inside a path token.
    if candidate.contains(|c: char| c.is_whitespace() || c == '\'' || c == '"') {
        return None;
    }
    let mut parts = candidate.split(['/', '\\']);
    if parts.next() != Some("spill") {
        return None;
    }
    let _label = parts.next()?;
    let file = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let stem = file.strip_suffix(".txt")?;
    let mut back = stem.rsplitn(3, '-');
    match (back.next(), back.next(), back.next()) {
        (Some(id), Some(secs), Some(_name)) => {
            let numeric = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit());
            (numeric(id) && numeric(secs)).then_some(end)
        }
        _ => None,
    }
}

/// Mask ISO-8601-ish stamps (`2026-09-05T11:22:33`) and bare `HH:MM:SS`
/// clock times. Applied only to the runtime-context snapshot, which is the
/// one message the harness itself writes a clock into.
fn mask_times(s: &str) -> String {
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = date_at(&bytes, i) {
            out.push_str("{{time}}");
            i += len;
            continue;
        }
        if let Some(len) = clock_at(&bytes, i) {
            out.push_str("{{time}}");
            i += len;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// `YYYY-MM-DD` plus an optional `THH:MM:SS`. Returns the length matched.
fn date_at(c: &[char], i: usize) -> Option<usize> {
    let digits = |at: usize, n: usize| {
        (at + n <= c.len()) && c[at..at + n].iter().all(|ch| ch.is_ascii_digit())
    };
    if !(digits(i, 4) && c.get(i + 4) == Some(&'-') && digits(i + 5, 2)
        && c.get(i + 7) == Some(&'-') && digits(i + 8, 2))
    {
        return None;
    }
    let mut len = 10;
    if c.get(i + 10) == Some(&'T') || c.get(i + 10) == Some(&' ') {
        if let Some(clock) = clock_at(c, i + 11) {
            len = 11 + clock;
        }
    }
    Some(len)
}

/// `HH:MM` with an optional `:SS`.
fn clock_at(c: &[char], i: usize) -> Option<usize> {
    let digits = |at: usize, n: usize| {
        (at + n <= c.len()) && c[at..at + n].iter().all(|ch| ch.is_ascii_digit())
    };
    if !(digits(i, 2) && c.get(i + 2) == Some(&':') && digits(i + 3, 2)) {
        return None;
    }
    let mut len = 5;
    if c.get(i + 5) == Some(&':') && digits(i + 6, 2) {
        len = 8;
    }
    Some(len)
}

/// A unified-ish diff of two normalised logs, line by line. Enough to say
/// *which* line diverged and how, without a diff dependency.
pub fn diff(expected: &str, actual: &str) -> Vec<String> {
    let e: Vec<&str> = expected.lines().collect();
    let a: Vec<&str> = actual.lines().collect();
    let mut out = Vec::new();
    for i in 0..e.len().max(a.len()) {
        match (e.get(i), a.get(i)) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                out.push(format!("line {}:\n  expected: {x}\n  actual:   {y}", i + 1));
            }
            (Some(x), None) => out.push(format!("line {}: missing, expected: {x}", i + 1)),
            (None, Some(y)) => out.push(format!("line {}: unexpected: {y}", i + 1)),
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_and_session_identity_are_tokenised() {
        let log = r#"{"seq":1,"ts":1757000000000,"type":"session_created","id":75,"title":"Session 75","created_at":1757000000}"#;
        let (out, _) = normalize(log, "");
        assert!(out.contains(r#""ts":"{{ts}}""#), "{out}");
        assert!(out.contains(r#""id":"{{session}}""#), "{out}");
        assert!(out.contains(r#""title":"{{session-title}}""#), "{out}");
        assert!(out.contains(r#""created_at":"{{ts}}""#), "{out}");
    }

    #[test]
    fn a_real_title_is_content_and_survives() {
        // Only the `Session N` placeholder is identity; a title the titler
        // or a human wrote is something a scenario may assert on.
        let log = r#"{"seq":2,"ts":1,"type":"session_created","id":4,"title":"list the crates","created_at":1}"#;
        let (out, _) = normalize(log, "");
        assert!(out.contains(r#""title":"list the crates""#), "{out}");
    }

    #[test]
    fn user_text_that_merely_looks_like_an_identifier_is_left_alone() {
        // dsh's rule, and the reason this is a field-by-field normaliser
        // rather than a regex over the whole line.
        let log = r#"{"seq":3,"ts":9,"type":"user_message","surface":{"op":"append"},"content":"session 75 failed at 12:30:01 in C:\\other\\place"}"#;
        let (out, _) = normalize(log, "C:\\work");
        assert!(out.contains("session 75 failed at 12:30:01"), "{out}");
        assert!(out.contains("C:"), "an unrelated path must survive: {out}");
    }

    #[test]
    fn the_working_directory_is_tokenised_in_both_separator_styles() {
        let log = r#"{"seq":4,"ts":1,"type":"tool_call","name":"read-file","args_preview":"C:\\work\\a.txt","expectation":"","args_json":"{\"path\":\"C:/work/a.txt\"}"}"#;
        let (out, _) = normalize(log, "C:\\work");
        assert!(!out.contains("work"), "the cwd should be gone: {out}");
        assert_eq!(out.matches("{{cwd}}").count(), 2, "{out}");
    }

    #[test]
    fn the_prompt_bodies_move_to_the_sidecar() {
        let log = r#"{"seq":5,"ts":1,"type":"request_envelope","fingerprint":12345,"system":"you are a blade","tools":"[]"}"#;
        let (out, sidecar) = normalize(log, "");
        assert!(out.contains(r#""system":"{{system}}""#), "{out}");
        assert!(out.contains(r#""tools":"{{tools}}""#), "{out}");
        assert!(out.contains(r#""fingerprint":"{{fingerprint}}""#), "{out}");
        assert_eq!(sidecar.envelopes.len(), 1);
        assert_eq!(sidecar.envelopes[0].0, 12345);
        assert_eq!(sidecar.envelopes[0].1, "you are a blade");
    }

    #[test]
    fn the_runtime_snapshot_loses_its_clock_but_keeps_the_rest() {
        let log = r#"{"seq":6,"ts":1,"type":"context_injected","surface":{"op":"append"},"source":{"kind":"runtime_context"},"content":"time: 2026-09-05T11:22:33\nmodel: qwen3\npermission: workspace-write"}"#;
        let (out, _) = normalize(log, "");
        assert!(out.contains("{{time}}"), "{out}");
        assert!(!out.contains("2026-09-05"), "{out}");
        assert!(out.contains("qwen3"), "the model is what the run used: {out}");
        assert!(out.contains("workspace-write"), "{out}");
    }

    #[test]
    fn two_runs_of_the_same_session_normalise_identically() {
        // The property the whole eval rests on.
        let a = r#"{"seq":1,"ts":1000,"type":"session_created","id":1,"title":"Session 1","created_at":1}
{"seq":2,"ts":1100,"type":"user_message","surface":{"op":"append"},"content":"hi"}"#;
        let b = r#"{"seq":1,"ts":9999,"type":"session_created","id":88,"title":"Session 88","created_at":9}
{"seq":2,"ts":9999,"type":"user_message","surface":{"op":"append"},"content":"hi"}"#;
        let (na, _) = normalize(a, "C:\\one");
        let (nb, _) = normalize(b, "C:\\two");
        assert_eq!(na, nb);
        assert!(diff(&na, &nb).is_empty());
    }

    #[test]
    fn the_retry_backoff_is_tokenised_but_the_reason_is_not() {
        let log = r#"{"seq":1,"ts":1,"type":"llm_retry","attempt":1,"max":5,"delay_ms":347,"reason":"empty response"}"#;
        let (out, _) = normalize(log, "");
        assert!(out.contains(r#""delay_ms":"{{delay}}""#), "{out}");
        assert!(out.contains(r#""reason":"empty response""#), "{out}");
        assert!(out.contains(r#""attempt":1"#), "{out}");
    }

    #[test]
    fn a_spill_filename_is_tokenised_but_a_plain_path_is_not() {
        // The timestamp and id in a spill filename move every run; the
        // rest of a path is content.
        let log = r#"{"seq":1,"ts":1,"type":"tool_result","surface":{"op":"append"},"call_seq":0,"skill":"run-cli","ok":true,"summary":"saved to C:/w/spill/7/run-cli-1757000000-3.txt; use read-file","trusted":true}"#;
        let (out, _) = normalize(log, "");
        assert!(out.contains("{{spill}}"), "{out}");
        assert!(!out.contains("1757000000"), "{out}");
    }

    #[test]
    fn a_txt_path_that_is_not_a_spill_file_survives() {
        let log = r#"{"seq":1,"ts":1,"type":"user_message","surface":{"op":"append"},"content":"open notes/spill-2024-01.txt please"}"#;
        let (out, _) = normalize(log, "");
        assert!(out.contains("notes/spill-2024-01.txt"), "{out}");
    }

    #[test]
    fn masking_the_cwd_leaves_the_log_deserialisable() {
        // The recording is the replay script, so `--bless` may only replace
        // the path — a fully tokenised `ts` would stop `from_log` parsing it.
        let log = r#"{"seq":1,"ts":1757000000000,"type":"tool_call","name":"read-file","args_preview":"C:\\w\\a.txt","expectation":""}"#;
        let out = mask_cwd(log, "C:\\w");
        assert!(out.contains("{{cwd}}"), "{out}");
        assert!(out.contains("1757000000000"), "the timestamp must survive: {out}");
        let v: Value = serde_json::from_str(out.trim()).expect("still valid JSON");
        assert_eq!(v["ts"], 1757000000000_i64);
    }

    #[test]
    fn the_longest_path_wins_when_one_contains_the_other() {
        // The scratch root is a prefix of the working directory. Tokenising
        // the root first would leave `{{root}}/work` and never produce the
        // `{{cwd}}` the recording holds.
        let log = r#"{"seq":1,"ts":1,"type":"user_message","surface":{"op":"append"},"content":"in C:/scratch/work and C:/scratch/spill"}"#;
        let (out, _) = normalize_paths(
            log,
            &[("C:/scratch", "{{root}}"), ("C:/scratch/work", "{{cwd}}")],
        );
        assert!(out.contains("{{cwd}}"), "{out}");
        assert!(out.contains("{{root}}/spill"), "{out}");
        assert!(!out.contains("scratch"), "{out}");
    }

    #[test]
    fn a_diff_names_the_line_and_shows_both_sides() {
        let d = diff("same\nleft\n", "same\nright\n");
        assert_eq!(d.len(), 1);
        assert!(d[0].starts_with("line 2:"), "{}", d[0]);
        assert!(d[0].contains("expected: left"), "{}", d[0]);
        assert!(d[0].contains("actual:   right"), "{}", d[0]);
    }

    #[test]
    fn extra_and_missing_lines_are_reported_as_such() {
        assert!(diff("a\n", "a\nb\n")[0].contains("unexpected"));
        assert!(diff("a\nb\n", "a\n")[0].contains("missing"));
    }

    #[test]
    fn a_torn_line_is_kept_verbatim_rather_than_dropped() {
        let (out, _) = normalize("{\"seq\":1,\"ts\":1,\"ty", "");
        assert!(out.contains("\"ty"), "{out}");
    }
}
