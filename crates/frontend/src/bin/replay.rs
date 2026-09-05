//! Recorded-session snapshot evals (guide §14.1). Sibling of `smoke`.
//!
//! ```powershell
//! .\run.ps1 run -p frontend --bin replay                       # every scenario
//! .\run.ps1 run -p frontend --bin replay -- spill-digest       # one
//! .\run.ps1 run -p frontend --bin replay -- --bless            # re-record
//! ```
//!
//! A scenario is a directory under `snapshots/`:
//!
//! ```text
//! snapshots/<name>/
//!   session.jsonl           the recording: user messages *and* the script
//!   scenario.toml           optional: prompt window, working-dir seed
//!   replay.override.json    optional: failures a log cannot express
//!   workspace/              optional: files the run starts with
//!   workspace.expected/     optional: files the run must end with
//! ```
//!
//! The driver spawns the backend with `--replay <dir>`, replays the recorded
//! user messages in order, and diffs the session log the run produced —
//! tokenised by `sica_core::snapshot` — against the recording. Everything
//! between the two is the real harness: retry classification, compaction,
//! the spill policy, the tool pipeline. That is the point of the eval; the
//! model is the only part that is faked.
//!
//! `workspace.expected/` exists because of the rule that motivates the whole
//! design: **model prose and tool-result text do not prove the external
//! effect.** A scenario that writes a file asserts on the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use futures::{SinkExt, StreamExt};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream as IpcStream},
    GenericNamespaced, ToNsName,
};
use protocol::{Event, Frame, Payload, Request, Response};
use sica_core::event::{EventKind, SessionEvent};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

/// How long one scenario's turns may take before the driver gives up. A
/// replayed turn does no I/O worth waiting for; anything near this is a
/// hang, and hanging is the failure mode a test must not have.
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// Silence after which a turn counts as finished.
///
/// `TurnFinished` is emitted per **hop**, not per turn — a tool call, a
/// retry and a compaction each end one — so waiting for the first of them
/// would cut the run off mid-turn and diff a truncated log. The honest
/// signal is quiet: no frames, with the retry backoff accounted for
/// explicitly below.
const SETTLE: Duration = Duration::from_millis(1_200);

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn backend_exe() -> PathBuf {
    let ext = if cfg!(windows) { ".exe" } else { "" };
    repo_root().join("target").join("debug").join(format!("backend{ext}"))
}

/// Per-scenario knobs. Everything has a default; a scenario that needs none
/// of them ships no file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
struct Scenario {
    /// Prompt window for the run. Small values make compaction fire on a
    /// short recording, which is the only way `compaction-replace` can be a
    /// scenario rather than a 200-message transcript.
    window: u32,
    /// One-line description, printed with the result.
    description: String,
    /// Extra copies of the recording's last completion to add to the script.
    ///
    /// Only for a scenario whose replies are interchangeable: compaction
    /// spends completions the log cannot record (a summary the policy
    /// retried, a compaction whose history moved underneath it), so without
    /// this a recording sheds one turn every time it is re-blessed.
    pad: usize,
    /// `text` | `native` | `ptc` — how the recorded run offered its tools
    /// (guide §7). A PTC recording is the only way to exercise the
    /// `run-code` path end to end, and the mode has to match the recording
    /// or every completion answers a differently-shaped request.
    tool_mode: String,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            window:      32_768,
            description: String::new(),
            pad:         0,
            tool_mode:   "text".into(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut bless = false;
    let mut only: Vec<String> = Vec::new();
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--bless" => bless = true,
            other if other.starts_with("--") => bail!("replay: unknown flag {other:?}"),
            other => only.push(other.to_string()),
        }
    }

    let exe = backend_exe();
    anyhow::ensure!(exe.exists(), "backend not built: {}", exe.display());
    let root = repo_root().join("snapshots");
    anyhow::ensure!(root.is_dir(), "no snapshots directory at {}", root.display());

    let mut scenarios: Vec<PathBuf> = std::fs::read_dir(&root)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("session.jsonl").is_file())
        .collect();
    scenarios.sort();
    if !only.is_empty() {
        scenarios.retain(|p| {
            only.iter().any(|n| p.file_name().is_some_and(|f| f == n.as_str()))
        });
        anyhow::ensure!(!scenarios.is_empty(), "no scenario matched {only:?}");
    }

    let mut failures = Vec::new();
    for dir in &scenarios {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        match run_scenario(&exe, dir, bless).await {
            Ok(()) => println!("replay: {name} OK"),
            Err(e) => {
                println!("replay: {name} FAILED\n{e}");
                failures.push(name);
            }
        }
    }

    if failures.is_empty() {
        println!("replay: {} scenario(s) OK", scenarios.len());
        Ok(())
    } else {
        bail!("replay: {} scenario(s) failed: {}", failures.len(), failures.join(", "))
    }
}

async fn run_scenario(exe: &Path, dir: &Path, bless: bool) -> Result<()> {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let recording = std::fs::read_to_string(dir.join("session.jsonl"))?;
    let scenario: Scenario = match std::fs::read_to_string(dir.join("scenario.toml")) {
        Ok(t) => toml::from_str(&t)?,
        Err(_) => Scenario::default(),
    };
    let prompts = user_messages(&recording);
    anyhow::ensure!(!prompts.is_empty(), "{name}: the recording holds no user message");
    if !scenario.description.is_empty() {
        println!("replay: {name} — {}", scenario.description);
    }

    // A scratch tree per run: sessions, spill and seeded skills land there,
    // so a scenario never sees the previous run's state or the checkout's.
    let scratch = scratch_dir(&name);
    let _ = std::fs::remove_dir_all(&scratch);
    let work = scratch.join("work");
    std::fs::create_dir_all(&work)?;
    if dir.join("workspace").is_dir() {
        copy_tree(&dir.join("workspace"), &work)?;
    }

    let pipe_name = format!(r"\\.\pipe\sica-rust-replay-{}-{name}", std::process::id());
    let mut child = Command::new(exe)
        .arg("--ipc")
        .arg(&pipe_name)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--replay")
        .arg(dir)
        .arg("--replay-window")
        .arg(scenario.window.to_string())
        .arg("--replay-pad")
        .arg(scenario.pad.to_string())
        .arg("--replay-tool-mode")
        .arg(&scenario.tool_mode)
        // Invariants on: a scenario that diverges from the log is exactly
        // what §14.3 watches for, and a run is the cheapest place to watch.
        .arg("--invariants")
        .env(sica_core::paths::WORKSPACE_ROOT_ENV, &scratch)
        .env(sica_core::paths::WORKING_DIR_ENV, &work)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let raw = pipe_name.strip_prefix(r"\\.\pipe\").unwrap();
    let mut stream: Option<IpcStream> = None;
    for _ in 0..80 {
        let ns = raw.to_ns_name::<GenericNamespaced>()?;
        match IpcStream::connect(ns).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let Some(stream) = stream else {
        bail!("{name}: could not connect to the backend");
    };
    let (r, w) = tokio::io::split(stream);
    let mut reader = FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = FramedWrite::new(w, LengthDelimitedCodec::new());

    writer
        .send(
            Frame {
                id: 0,
                payload: Payload::ClientHello {
                    protocol_version: protocol::PROTOCOL_VERSION,
                },
            }
            .encode()?
            .into(),
        )
        .await?;

    let mut errors: Vec<String> = Vec::new();
    // Assigned by the `SessionCreated` answer below; the loop cannot fall
    // through without it.
    let session_id: u64;
    let mut next_id = 1u64;

    // New session.
    writer
        .send(Frame::request(next_id, Request::NewSession).encode()?.into())
        .await?;
    let id = next_id;
    next_id += 1;
    loop {
        let frame = read_frame(&mut reader).await?;
        collect_errors(&frame, &mut errors);
        if let Payload::Response(Response::SessionCreated { id: sid }) = &frame.payload {
            if frame.id == id {
                session_id = *sid;
                break;
            }
        }
    }

    // Replay the recorded prompts, one turn at a time.
    for text in &prompts {
        writer
            .send(
                Frame::request(next_id, Request::SendUserMessage {
                    session_id,
                    text: text.clone(),
                    images: Vec::new(),
                })
                .encode()?
                .into(),
            )
            .await?;
        next_id += 1;
        let hard = tokio::time::Instant::now() + TURN_TIMEOUT;
        let mut quiet = tokio::time::Instant::now() + SETTLE;
        let mut saw_finish = false;
        loop {
            if tokio::time::Instant::now() >= hard {
                bail!("{name}: a turn did not finish within {TURN_TIMEOUT:?}");
            }
            let until = quiet.min(hard);
            let frame = match tokio::time::timeout_at(until, read_frame(&mut reader)).await {
                Ok(f) => f?,
                // Quiet for a whole settle window with at least one hop
                // finished: the turn is over.
                Err(_) if saw_finish => break,
                Err(_) => bail!("{name}: the backend went quiet before any hop finished"),
            };
            collect_errors(&frame, &mut errors);
            quiet = tokio::time::Instant::now() + SETTLE;
            match &frame.payload {
                Payload::Event(Event::TurnFinished { session_id: sid, .. })
                    if *sid == session_id =>
                {
                    saw_finish = true;
                }
                // The backoff is a deliberate sleep, not a stall. Extend
                // the quiet window by exactly what the backend announced.
                Payload::Event(Event::LlmRetry { delay_ms, .. }) => {
                    quiet += Duration::from_millis(*delay_ms);
                }
                _ => {}
            }
        }
    }

    writer
        .send(Frame::request(next_id, Request::Shutdown).encode()?.into())
        .await?;
    let _ = writer.get_mut().shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    // ---- the assertions -------------------------------------------------

    let produced_path = scratch.join("sessions").join(format!("{session_id}.jsonl"));
    let produced = std::fs::read_to_string(&produced_path)
        .map_err(|e| anyhow::anyhow!("{name}: no session log at {}: {e}", produced_path.display()))?;

    if bless {
        // The path this run happened to use is the one thing a recording
        // must not carry: every later replay runs somewhere else. Only the
        // path — the rest stays raw, because the recording is also the
        // replay script and has to deserialise.
        let masked = sica_core::snapshot::mask_paths(&produced, &[
            (work.display().to_string().as_str(), "{{cwd}}"),
            (scratch.display().to_string().as_str(), "{{root}}"),
        ]);
        std::fs::write(dir.join("session.jsonl"), &masked)?;
        println!("replay: {name} blessed ({} bytes)", masked.len());
        return Ok(());
    }

    let (want, _) = sica_core::snapshot::normalize(&recording, "");
    // Two paths, because a run's state is split between them: the agent
    // acts on `work`, and its sessions and spill files live under the
    // scratch root. Both carry this run's pid.
    let (got, _) = sica_core::snapshot::normalize_paths(&produced, &[
        (work.display().to_string().as_str(), "{{cwd}}"),
        (scratch.display().to_string().as_str(), "{{root}}"),
    ]);
    let d = sica_core::snapshot::diff(&want, &got);
    if !d.is_empty() {
        let shown: Vec<String> = d.iter().take(8).cloned().collect();
        errors.push(format!(
            "the replayed log diverges from the recording ({} line(s)):\n{}",
            d.len(),
            shown.join("\n")
        ));
    }

    // The external effect, when the scenario claims one. Model prose and
    // tool-result text do not prove it.
    let expected_ws = dir.join("workspace.expected");
    if expected_ws.is_dir() {
        errors.extend(diff_tree(&expected_ws, &work));
    }

    if errors.is_empty() {
        let _ = std::fs::remove_dir_all(&scratch);
        Ok(())
    } else {
        // Kept on failure: the scratch tree is the evidence.
        bail!("{}\n(scratch kept at {})", errors.join("\n"), scratch.display())
    }
}

/// Backend ERROR lines are failures in their own right — including the
/// invariant companions', which is why the run turns them on.
fn collect_errors(frame: &Frame, out: &mut Vec<String>) {
    if let Payload::Event(Event::LogLine { level, message }) = &frame.payload {
        if level == "ERROR" {
            out.push(format!("backend ERROR: {message}"));
        }
    }
}

async fn read_frame<R>(reader: &mut FramedRead<R, LengthDelimitedCodec>) -> Result<Frame>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let bytes = reader
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("backend closed the pipe"))??;
    Ok(Frame::decode(&bytes)?)
}

/// The user messages of a recording, in order — the prompts to replay.
/// Only `UserMessage`; injected context is something the harness produces
/// for itself, and feeding it back as a prompt would double it.
fn user_messages(jsonl: &str) -> Vec<String> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<SessionEvent>(l).ok())
        .filter_map(|ev| match ev.kind {
            EventKind::UserMessage { content, .. } => Some(content),
            _ => None,
        })
        .collect()
}

/// The scratch tree for one scenario. The pid keeps concurrent drivers
/// apart, and it is **zero-padded to a fixed width** because this path ends
/// up inside the prompt: a spill notice names the file it spilled to, so a
/// 4-digit pid and a 5-digit one price the same history one token apart and
/// the recording's `token_usage` line fails on every other run.
fn scratch_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sica-replay-{:010}-{name}", std::process::id()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Compare `expected` against `actual` file by file. Extra files in
/// `actual` are *not* a failure: a scenario asserts what it cares about,
/// and the harness legitimately leaves spill files and seeded skills
/// behind. A missing or differing expected file is.
fn diff_tree(expected: &Path, actual: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut want = BTreeMap::new();
    collect_files(expected, expected, &mut want);
    for (rel, body) in want {
        let path = actual.join(&rel);
        match std::fs::read_to_string(&path) {
            Ok(got) if normalize_newlines(&got) == normalize_newlines(&body) => {}
            Ok(got) => out.push(format!(
                "workspace: {} differs\n  expected: {:?}\n  actual:   {:?}",
                rel.display(),
                truncate(&body),
                truncate(&got)
            )),
            Err(e) => out.push(format!("workspace: {} missing ({e})", rel.display())),
        }
    }
    out
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_files(root, &p, out);
        } else if let Ok(body) = std::fs::read_to_string(&p) {
            if let Ok(rel) = p.strip_prefix(root) {
                out.insert(rel.to_path_buf(), body);
            }
        }
    }
}

/// The repo is CRLF and a tool writes LF; that difference is never what a
/// scenario is about.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    s.chars().take(200).collect::<String>() + "…"
}
