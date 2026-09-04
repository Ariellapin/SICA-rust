use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use protocol::{Event, Frame, Payload, Request};
use sica_core::paths::{
    agents_dir, commands_dir, evals_dir, memory_file, skills_dir, workspace_root,
};

mod be_core;
mod catalog;
mod chat;
mod dispatcher;
mod inbox;
mod jobs_bridge;
mod ipc;
mod parent_watch;
mod sessions_store;
mod title_gen;

use be_core::BeState;
use chat::ChatHub;

#[derive(Debug, Clone)]
struct Args {
    ipc: String,
    parent_pid: Option<u32>,
}

fn parse_args() -> Args {
    let mut ipc = None;
    let mut parent_pid = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--ipc" => ipc = it.next(),
            "--parent-pid" => parent_pid = it.next().and_then(|s| s.parse().ok()),
            "--log-level" => {
                if let Some(lvl) = it.next() {
                    std::env::set_var("RUST_LOG", lvl);
                }
            }
            other => eprintln!("backend: unknown arg {other:?}"),
        }
    }
    Args {
        ipc: ipc.expect("--ipc <pipe-name> is required"),
        parent_pid,
    }
}

fn main() -> Result<()> {
    let args = parse_args();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run(args))
}

async fn run(args: Args) -> Result<()> {
    info!(pipe = %args.ipc, "backend starting");

    if let Some(ppid) = args.parent_pid {
        parent_watch::spawn(ppid);
    }

    let (read_half, write_half) = ipc::accept(&args.ipc).await?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Frame>();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Frame>();

    // Reader task: socket -> in_tx
    let read_task = tokio::spawn(async move {
        let mut framed = ipc::framed_read(read_half);
        while let Some(item) = framed.next().await {
            match item {
                Ok(bytes) => match Frame::decode(&bytes) {
                    Ok(f) => {
                        if in_tx.send(f).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!(error = %e, "decode error"),
                },
                Err(e) => {
                    warn!(error = %e, "read error");
                    break;
                }
            }
        }
        info!("reader task ended");
    });

    // Writer task: out_rx -> socket
    let write_task = tokio::spawn(async move {
        let mut framed = ipc::framed_write(write_half);
        while let Some(frame) = out_rx.recv().await {
            match frame.encode() {
                Ok(bytes) => {
                    if let Err(e) = framed.send(bytes.into()).await {
                        warn!(error = %e, "write error");
                        break;
                    }
                }
                Err(e) => warn!(error = %e, "encode error"),
            }
        }
        info!("writer task ended");
    });

    let state = Arc::new(BeState::new());
    let started = Instant::now();

    // Skill registry — seed the built-in skills' markdown contracts and
    // the workspace `memory.md` index, then scan the folder so any user-
    // authored `*.md` is loaded alongside them.
    let skills_path = skills_dir();
    let root = workspace_root();
    if let Err(e) = agents::skill_creator::seed_default(&skills_path) {
        warn!(error = %e, dir = %skills_path.display(), "seed skill-creator.md failed");
    }
    if let Err(e) = agents::builtins::seed_defaults(&skills_path) {
        warn!(error = %e, dir = %skills_path.display(), "seed builtin skill docs failed");
    }
    let evals_path = evals_dir();
    if let Err(e) = agents::model_eval::seed_defaults(&skills_path, &evals_path) {
        warn!(error = %e, dir = %evals_path.display(), "seed model-eval suite failed");
    }
    let memory_path = memory_file();
    if let Err(e) = agents::memory::seed_default(&memory_path) {
        warn!(error = %e, path = %memory_path.display(), "seed memory.md failed");
    }
    // The plan-mode policy is user-editable configuration: seed once,
    // never overwrite. It lives in `skills/` next to the docs but is
    // excluded from the skill scan by name (`md_skill::register_all`).
    if let Err(e) = agents::control::seed_plan_mode(&skills_path) {
        warn!(error = %e, dir = %skills_path.display(), "seed plan-mode.md failed");
    }
    // `agents/` and `commands/` back the other two families of the FE's "/"
    // palette. Created empty so the folders are discoverable; the palette
    // simply lists nothing for a family with no files in it.
    for dir in [agents_dir(), commands_dir()] {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(error = %e, dir = %dir.display(), "create palette dir failed");
        }
    }
    // Background jobs (Wave 4, guide §12.4). Built before the registry
    // because the shell skills hold it: `run-cli 'cargo build'
    // 'background=true'` starts a job instead of waiting. The hub adopts the
    // same instance below (`with_jobs`) and the bridge wires completions
    // back into the session.
    let jobs = Arc::new(agents::JobRegistry::new());
    let mut skill_registry = agents::SkillRegistry::new();
    skill_registry.register(Arc::new(agents::SkillCreator::new(skills_path.clone())));
    skill_registry.register(Arc::new(agents::RunCli(Some(jobs.clone()))));
    skill_registry.register(Arc::new(agents::RunPwsh(Some(jobs.clone()))));
    skill_registry.register(Arc::new(agents::JobOutput(jobs.clone())));
    skill_registry.register(Arc::new(agents::JobList(jobs.clone())));
    skill_registry.register(Arc::new(agents::JobKill(jobs.clone())));
    skill_registry.register(Arc::new(agents::ReadFile::new(root.clone())));
    skill_registry.register(Arc::new(agents::WriteFile::new(root.clone())));
    skill_registry.register(Arc::new(agents::EditFile::new(root.clone())));
    skill_registry.register(Arc::new(agents::Glob::new(root.clone())));
    skill_registry.register(Arc::new(agents::Grep::new(root.clone())));
    // `ask-user` needs the broker the hub installs on every sub-agent.
    // `todo-write` / `exit-plan-mode` are catalogue entries whose bodies
    // run in the hub (session-log mutation + turn control) — the
    // dispatcher intercepts them before any sub-agent spins up.
    skill_registry.register(Arc::new(agents::AskUser));
    skill_registry.register(Arc::new(agents::control::TodoWrite));
    skill_registry.register(Arc::new(agents::control::ExitPlanMode));
    // `model-eval` benchmarks the connected model against a prompt suite. It
    // needs the finished registry (for the live catalogue and the known-skill
    // predicate its tool-call checks use), so it is attached below alongside
    // `agent-team`. It never dispatches a skill — tool-call cases are
    // parse-only — so a suite is safe to run unattended.
    let model_eval = Arc::new(agents::ModelEval::new(root.clone()));
    skill_registry.register(model_eval.clone());
    // Delegation (Wave 4, guide §12.1, §12.6): `subagent` runs one bounded
    // task in a fresh conversation, `subagent-fork` in one seeded with this
    // session's completed turns, and `ralph` runs fresh rounds against a
    // fixed objective. All three drive full LLM conversations, so like
    // `agent-team` they need the finished registry — attached below.
    let subagent = Arc::new(agents::Subagent::fresh());
    let subagent_fork = Arc::new(agents::Subagent::forking());
    let ralph = Arc::new(agents::Ralph::new());
    skill_registry.register(subagent.clone());
    skill_registry.register(subagent_fork.clone());
    skill_registry.register(ralph.clone());
    // `agent-team` is opt-in: it registers only when the user has put
    // `skills/agent-team.md` on disk. A team is N full LLM conversations per
    // call and its teammates are the least reliable output in the app on a
    // small local model, so it stays out of the catalogue — and out of the
    // model's reach — until someone asks for it. Deleting the doc turns it
    // off again at the next backend start.
    //
    // Registered before the markdown scan so that same doc can't shadow the
    // real skill (`register_if_absent` in `register_all`).
    let team_doc = skills_path.join(format!("{}.md", agents::team::AGENT_TEAM_NAME));
    let agent_team = team_doc.exists().then(|| {
        let team = Arc::new(agents::AgentTeam::new());
        skill_registry.register(team.clone());
        team
    });
    let parse_errors = agents::md_skill::register_all(&mut skill_registry, &skills_path);
    let skill_count = skill_registry.by_name.len();
    let skill_registry = Arc::new(skill_registry);
    // The team skill needs the finished registry so its teammates can call
    // other skills; attached as a Weak because the registry also owns it.
    if let Some(team) = &agent_team {
        team.attach_registry(&skill_registry);
    }
    model_eval.attach_registry(&skill_registry);
    subagent.attach_registry(&skill_registry);
    subagent_fork.attach_registry(&skill_registry);
    ralph.attach_registry(&skill_registry);
    info!(
        count = skill_count,
        dir = %skills_path.display(),
        agent_team = agent_team.is_some(),
        "skills loaded"
    );
    let _ = out_tx.send(Frame::event(Event::LogLine {
        level: "INFO".into(),
        message: format!(
            "skills: {skill_count} loaded from {}",
            skills_path.display()
        ),
    }));
    let _ = out_tx.send(Frame::event(Event::LogLine {
        level: "INFO".into(),
        message: if agent_team.is_some() {
            format!(
                "agent-team: enabled ({} present) — to disable, rename it to \
                 agent-team.md.off (only *.md is scanned) or delete it, then \
                 restart the backend",
                team_doc.display()
            )
        } else {
            format!(
                "agent-team: disabled — put a file at {} and restart the \
                 backend to enable it",
                team_doc.display()
            )
        },
    }));
    for (path, err) in parse_errors {
        warn!(file = %path.display(), error = %err, "skill parse error");
        let _ = out_tx.send(Frame::event(Event::LogLine {
            level: "WARN".into(),
            message: format!("skill parse error in {}: {err}", path.display()),
        }));
    }

    // Idealist daemon comes up *before* the chat hub: ChatHub needs the bus
    // so it can install a `ToolFailureSink` on every sub-agent it spawns.
    // Every event flows out through the same write task.
    let idealist_sink: Arc<dyn idealist::IdealistEventSink> = Arc::new(OutSink { tx: out_tx.clone() });
    let idealist = Arc::new(idealist::Idealist::new(idealist_sink));
    let idealist_bus = idealist.bus.clone();
    Arc::clone(&idealist).spawn();

    // Bridge: agents::ToolFailureSink → idealist::TriggerBus. Each failed
    // sub-agent tool call becomes a `Trigger` the idealist daemon picks up.
    let tool_failure_sink: Arc<dyn agents::ToolFailureSink> =
        Arc::new(ToolFailureBridge { bus: idealist_bus.clone() });

    let chat = ChatHub::new_loaded(
        out_tx.clone(),
        skill_registry.clone(),
        Some(tool_failure_sink.clone()),
    )
    .with_jobs(jobs.clone());

    // Bridge: a finished job appends its audit line, drops a notice into the
    // owning session's inbox (the model reads it at its next step) and
    // refreshes the FE's list.
    jobs.attach_notifier(Arc::new(jobs_bridge::JobsBridge::new(&chat)));

    // Initial broadcasts: ServerHello + initial LLM state so the FE can sync.
    let _ = out_tx.send(Frame {
        id: 0,
        payload: Payload::ServerHello {
            protocol_version: protocol::PROTOCOL_VERSION,
            pid: std::process::id(),
            version: sica_core::build_id::source_version(),
        },
    });
    let _ = out_tx.send(Frame::event(Event::LlmStateChanged {
        state: protocol::LlmState::Disconnected,
    }));

    // Heartbeat task. FE silently consumes these to keep the IPC dot green;
    // it no longer logs them to the user-visible log panel.
    let hb_state = state.clone();
    let hb_tx = out_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await;
        loop {
            tick.tick().await;
            let uptime = started.elapsed().as_secs();
            let counter = hb_state.counter.get();
            if hb_tx
                .send(Frame::event(Event::Heartbeat { uptime_secs: uptime, counter }))
                .is_err()
            {
                break;
            }
        }
    });

    // Dispatcher loop.
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            maybe_frame = in_rx.recv() => {
                let Some(frame) = maybe_frame else { break };
                match frame.payload {
                    Payload::Request(req) => {
                        let is_shutdown = matches!(req, Request::Shutdown);
                        let resp = dispatcher::handle(req, &state, &chat, &idealist_bus, &shutdown_tx).await;
                        let _ = out_tx.send(Frame::response(frame.id, resp));
                        if is_shutdown { break; }
                    }
                    Payload::Ping => {
                        let _ = out_tx.send(Frame { id: frame.id, payload: Payload::Pong });
                    }
                    Payload::ClientHello { protocol_version } => {
                        info!(client_version = protocol_version, "client hello");
                    }
                    other => warn!(?other, "unexpected payload from client"),
                }
            }
            _ = shutdown_rx.recv() => {
                info!("shutdown requested");
                break;
            }
        }
    }

    drop(out_tx);
    heartbeat_task.abort();
    read_task.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), write_task).await;
    info!("backend exiting cleanly");
    Ok(())
}

struct OutSink {
    tx: mpsc::UnboundedSender<Frame>,
}

impl idealist::IdealistEventSink for OutSink {
    fn emit(&self, ev: Event) {
        let _ = self.tx.send(Frame::event(ev));
    }
}

/// Forwards each sub-agent tool-call failure into the idealist daemon. The
/// `module` field follows the `agents::tool::<skill-name>` convention so the
/// idealist `classify` function routes it to `TriggerSource::SubAgentTool`
/// and the analyzer can suggest an environment-appropriate replacement.
struct ToolFailureBridge {
    bus: idealist::TriggerBus,
}

impl agents::ToolFailureSink for ToolFailureBridge {
    fn report(&self, r: agents::ToolFailureReport) {
        let traceback = Some(format!(
            "host_os={}\nhost_family={}\ndepth={}\nargs={}",
            r.host_os, r.host_family, r.depth, r.args_preview,
        ));
        self.bus.publish(idealist::Trigger {
            kind:    "tool_failed".into(),
            module:  format!("agents::tool::{}", r.skill),
            message: r.summary,
            traceback,
        });
    }
}
