//! Spawn / kill the BE child process. Pipes stdout/stderr to UI as log lines.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::supervisor::{UiBridge, UiEvent};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct BeChild {
    pub pid: u32,
    inner: Child,
}

impl BeChild {
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.inner.wait().await
    }

    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.inner.start_kill()
    }
}

pub fn workspace_root() -> Result<PathBuf> {
    // CARGO_MANIFEST_DIR at build time of the frontend crate is .../crates/frontend
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .context("resolve workspace root")
}

pub fn backend_exe_path(release: bool) -> Result<PathBuf> {
    let root = workspace_root()?;
    let profile = if release { "release" } else { "debug" };
    let ext = if cfg!(windows) { ".exe" } else { "" };
    Ok(root.join("target").join(profile).join(format!("backend{ext}")))
}

pub async fn spawn(
    exe: &PathBuf,
    pipe_name: &str,
    parent_pid: u32,
    bridge: Arc<UiBridge>,
) -> Result<BeChild> {
    let mut cmd = Command::new(exe);
    cmd.arg("--ipc")
        .arg(pipe_name)
        .arg("--parent-pid")
        .arg(parent_pid.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(windows)]
    {
        // tokio::process::Command exposes creation_flags directly on Windows.
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().with_context(|| format!("spawn {}", exe.display()))?;
    let pid = child.id().context("child has no pid")?;

    // Pipe stdout/stderr to the UI.
    if let Some(out) = child.stdout.take() {
        let b = bridge.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                b.send(UiEvent::Log(format!("[BE.out] {line}")));
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let b = bridge.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                b.send(UiEvent::Log(format!("[BE.err] {line}")));
            }
        });
    }

    Ok(BeChild { pid, inner: child })
}
