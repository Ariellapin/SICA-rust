//! `cargo build` runner. Streams stdout+stderr line by line to the UI.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::child::workspace_root;
use crate::supervisor::{UiBridge, UiEvent};

pub async fn run_cargo_build(release: bool, bridge: Arc<UiBridge>) -> Result<bool> {
    let root = workspace_root()?;
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&root)
        .arg("build")
        .arg("-p")
        .arg("backend")
        .arg("--color=never")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if release {
        cmd.arg("--release");
    }

    bridge.send(UiEvent::BuildLine(format!(
        "$ cargo build -p backend{}",
        if release { " --release" } else { "" }
    )));

    let mut child = cmd.spawn().context("spawn cargo")?;

    if let Some(out) = child.stdout.take() {
        let b = bridge.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                b.send(UiEvent::BuildLine(line));
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let b = bridge.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                b.send(UiEvent::BuildLine(line));
            }
        });
    }

    let status = child.wait().await.context("wait cargo")?;
    Ok(status.success())
}
