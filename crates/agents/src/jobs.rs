//! Background work with a stable handle (guide §12.4).
//!
//! A tool call is synchronous: the turn waits for it, and `run-cli` caps
//! that wait at 30 s. That is the right shape for `git status` and the
//! wrong one for `cargo build` — which is exactly the command an agent most
//! wants to start and then get on with something else.
//!
//! A job is that second shape. `run-cli 'cargo build' 'background=true'`
//! returns a handle immediately (`started job cli-3`), the child keeps
//! running under the registry, and three generic tools cover it from then
//! on: [`JobOutput`] returns everything printed since the last read and
//! ends with `[status: …]`, [`JobList`] shows what is running, [`JobKill`]
//! stops one. The same three would serve a PTY or a detached subagent, so
//! nothing here is shell-specific beyond the spawner.
//!
//! Two properties are load-bearing:
//!
//! - **Jobs are per session.** Ids are only meaningful, and only visible,
//!   inside the session that started them. One agent cannot read or kill
//!   another's work.
//! - **Completion is pushed, not polled.** A finished job calls the
//!   [`JobNotifier`] the backend installs, which puts a note in the
//!   session's inbox; the model reads it at its next step whether or not
//!   it thought to ask. An agent that has to remember to poll will forget,
//!   and then report on a build it never saw the end of.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::skill::{Skill, SkillContext, SkillOutcome};

/// Jobs one session may have at once. A model that starts an eleventh has
/// almost certainly lost track of the ten already running.
pub const MAX_JOBS_PER_SESSION: usize = 10;

/// Output retained per job. Older bytes are dropped from the front; a read
/// that lost some says so rather than silently skipping it.
pub const OUTPUT_CAP: usize = 256 * 1024;

/// Most output handed back in one `job-output` call. The rest stays for
/// the next read.
pub const READ_CAP: usize = 32 * 1024;

pub const JOB_OUTPUT_NAME: &str = "job-output";
pub const JOB_LIST_NAME: &str = "job-list";
pub const JOB_KILL_NAME: &str = "job-kill";

/// Where a job is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Exited { code: i32 },
    Killed,
    Failed { error: String },
}

impl JobStatus {
    pub fn label(&self) -> String {
        match self {
            JobStatus::Running => "running".into(),
            JobStatus::Exited { code } => format!("exited {code}"),
            JobStatus::Killed => "killed".into(),
            JobStatus::Failed { error } => format!("failed: {error}"),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, JobStatus::Running)
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            JobStatus::Exited { code } => Some(*code),
            _ => None,
        }
    }
}

/// One background job. `produced` counts every byte the child has written;
/// `read_mark` is how far the model has been shown. Keeping both as
/// absolute counters means a read can tell "nothing new" from "output
/// scrolled past the cap before you asked".
struct Job {
    id:        String,
    kind:      String,
    command:   String,
    status:    JobStatus,
    buf:       Vec<u8>,
    produced:  u64,
    read_mark: u64,
    cancel:    CancellationToken,
}

impl Job {
    fn append(&mut self, chunk: &[u8]) {
        self.produced += chunk.len() as u64;
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > OUTPUT_CAP {
            let excess = self.buf.len() - OUTPUT_CAP;
            self.buf.drain(..excess);
        }
    }

    /// Everything not yet handed back, plus how many bytes were lost to the
    /// cap before the caller got to them.
    fn take_unread(&mut self) -> (String, u64) {
        let buf_start = self.produced - self.buf.len() as u64;
        let lost = buf_start.saturating_sub(self.read_mark);
        let from = self.read_mark.max(buf_start);
        let slice = &self.buf[(from - buf_start) as usize..];
        let text = String::from_utf8_lossy(slice).to_string();
        self.read_mark = self.produced;
        (text, lost)
    }

    fn summary(&self) -> JobSummary {
        JobSummary {
            id:      self.id.clone(),
            kind:    self.kind.clone(),
            command: self.command.clone(),
            status:  self.status.clone(),
            // Bytes the model has not read yet.
            unread:  self.produced - self.read_mark,
        }
    }
}

/// A job as the backend and the FE see it.
#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id:      String,
    pub kind:    String,
    pub command: String,
    pub status:  JobStatus,
    /// Bytes produced but not yet handed to the model.
    pub unread:  u64,
}

/// How the registry tells the rest of the app that something happened.
///
/// The `agents` crate cannot reach the backend's session inbox, so the
/// backend installs one of these at startup: `finished` puts a note in the
/// owning session's inbox (the model reads it at its next step), `changed`
/// refreshes the frontend's list.
pub trait JobNotifier: Send + Sync {
    fn finished(&self, session_id: u64, job: &JobSummary);
    fn changed(&self, session_id: u64);
}

/// Every session's background work.
#[derive(Default)]
pub struct JobRegistry {
    by_session: Mutex<HashMap<u64, Vec<Job>>>,
    next:       AtomicU64,
    notifier:   OnceLock<Arc<dyn JobNotifier>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the backend's bridge. Calling twice is a no-op — the first
    /// wins, which is what a startup-time wiring wants.
    pub fn attach_notifier(&self, notifier: Arc<dyn JobNotifier>) {
        let _ = self.notifier.set(notifier);
    }

    /// Start `cmd` in the background and return its handle.
    ///
    /// The child is `kill_on_drop` and, on Windows, in a kill-on-close Job
    /// Object, exactly like a foreground shell call — a background
    /// `npm install` that is killed must still take `node` with it.
    pub fn start_shell(
        self: &Arc<Self>,
        session_id: u64,
        kind: &str,
        command: &str,
        mut cmd: Command,
    ) -> Result<String, String> {
        {
            let g = self.by_session.lock().expect("jobs mutex");
            let running = g
                .get(&session_id)
                .map(|v| v.iter().filter(|j| j.status.is_running()).count())
                .unwrap_or(0);
            if running >= MAX_JOBS_PER_SESSION {
                return Err(format!(
                    "this session already has {running} background job(s) running \
                     (limit {MAX_JOBS_PER_SESSION}) — read or kill one first"
                ));
            }
        }

        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("spawn {kind}: {e}"))?;
        let guard = crate::proc::JobGuard::attach(&child);

        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("{kind}-{n}");
        let cancel = CancellationToken::new();
        {
            let mut g = self.by_session.lock().expect("jobs mutex");
            g.entry(session_id).or_default().push(Job {
                id:        id.clone(),
                kind:      kind.to_string(),
                command:   command.to_string(),
                status:    JobStatus::Running,
                buf:       Vec::new(),
                produced:  0,
                read_mark: 0,
                cancel:    cancel.clone(),
            });
        }

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let registry = Arc::clone(self);
        let job_id = id.clone();
        tokio::spawn(async move {
            // Holding the guard inside the task ties the Job Object's
            // lifetime to the child's, not to the call that started it.
            let _guard = guard;
            let mut out_buf = [0u8; 8192];
            let mut err_buf = [0u8; 8192];
            let status = loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = child.kill().await;
                        break JobStatus::Killed;
                    }
                    n = read_some(&mut stdout, &mut out_buf) => {
                        match n {
                            Some(0) | None => { stdout = None; }
                            Some(n) => registry.append(session_id, &job_id, &out_buf[..n]),
                        }
                    }
                    n = read_some(&mut stderr, &mut err_buf) => {
                        match n {
                            Some(0) | None => { stderr = None; }
                            Some(n) => registry.append(session_id, &job_id, &err_buf[..n]),
                        }
                    }
                    res = child.wait(), if stdout.is_none() && stderr.is_none() => {
                        break match res {
                            Ok(st) => JobStatus::Exited { code: st.code().unwrap_or(-1) },
                            Err(e) => JobStatus::Failed { error: e.to_string() },
                        };
                    }
                }
            };
            registry.settle(session_id, &job_id, status);
        });

        info!(session_id, job = %id, %command, "job started");
        self.notify_changed(session_id);
        Ok(id)
    }

    fn append(&self, session_id: u64, id: &str, chunk: &[u8]) {
        let mut g = self.by_session.lock().expect("jobs mutex");
        if let Some(job) = g.get_mut(&session_id).and_then(|v| find(v, id)) {
            job.append(chunk);
        }
    }

    /// Record a job's final status and push the completion notice.
    fn settle(&self, session_id: u64, id: &str, status: JobStatus) {
        let summary = {
            let mut g = self.by_session.lock().expect("jobs mutex");
            let Some(job) = g.get_mut(&session_id).and_then(|v| find(v, id)) else {
                return;
            };
            job.status = status;
            job.summary()
        };
        info!(session_id, job = %id, status = %summary.status.label(), "job finished");
        if let Some(n) = self.notifier.get() {
            n.finished(session_id, &summary);
            n.changed(session_id);
        }
    }

    fn notify_changed(&self, session_id: u64) {
        if let Some(n) = self.notifier.get() {
            n.changed(session_id);
        }
    }

    /// Everything this session has started, oldest first.
    pub fn list(&self, session_id: u64) -> Vec<JobSummary> {
        let g = self.by_session.lock().expect("jobs mutex");
        g.get(&session_id)
            .map(|v| v.iter().map(Job::summary).collect())
            .unwrap_or_default()
    }

    /// Output since the last read, its status line, and how much was lost
    /// to the retention cap.
    pub fn read(&self, session_id: u64, id: &str) -> Option<(String, u64, JobStatus)> {
        let mut g = self.by_session.lock().expect("jobs mutex");
        let job = g.get_mut(&session_id).and_then(|v| find(v, id))?;
        let (text, lost) = job.take_unread();
        Some((text, lost, job.status.clone()))
    }

    /// Ask a running job to stop. The watcher task kills the child and
    /// settles the status, so the notice goes out the usual way.
    pub fn kill(&self, session_id: u64, id: &str) -> Result<(), String> {
        let g = self.by_session.lock().expect("jobs mutex");
        let job = g
            .get(&session_id)
            .and_then(|v| v.iter().find(|j| j.id == id))
            .ok_or_else(|| format!("no job `{id}` in this session"))?;
        if !job.status.is_running() {
            return Err(format!("job `{id}` already {}", job.status.label()));
        }
        job.cancel.cancel();
        Ok(())
    }

    /// Stop everything a session started and forget it (the session was
    /// deleted). Jobs also die with the process.
    pub fn clear(&self, session_id: u64) {
        let mut g = self.by_session.lock().expect("jobs mutex");
        if let Some(jobs) = g.remove(&session_id) {
            for job in jobs {
                job.cancel.cancel();
            }
        }
    }
}

fn find<'a>(jobs: &'a mut [Job], id: &str) -> Option<&'a mut Job> {
    jobs.iter_mut().find(|j| j.id == id)
}

/// Read from a pipe that may already be closed. `None` means "this stream
/// is done"; the select arm then stops polling it.
async fn read_some<R>(stream: &mut Option<R>, buf: &mut [u8]) -> Option<usize>
where
    R: AsyncReadExt + Unpin,
{
    match stream {
        Some(s) => s.read(buf).await.ok(),
        // Nothing left to read: park this arm forever so `select!` falls
        // through to the wait arm instead of spinning on a ready `None`.
        None => std::future::pending().await,
    }
}

/// The sentence a finished job puts into its session's inbox.
pub fn completion_notice(job: &JobSummary) -> String {
    format!(
        "Background job `{}` ({}) {}. Command: {}\n\
         {} — read it with `{} '{}' > what it printed`.",
        job.id,
        job.kind,
        job.status.label(),
        job.command,
        if job.unread > 0 {
            format!("{} byte(s) of output are unread", job.unread)
        } else {
            "It produced no unread output".to_string()
        },
        JOB_OUTPUT_NAME,
        job.id,
    )
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// Shared handle every job skill needs. Held as an `Arc` by the registry
/// entry itself, so no `Weak` dance is required — `JobRegistry` does not
/// own any skill.
pub struct JobOutput(pub Arc<JobRegistry>);
pub struct JobList(pub Arc<JobRegistry>);
pub struct JobKill(pub Arc<JobRegistry>);

/// Jobs belong to a session; a call with no session (a teammate, an eval)
/// has nothing it could legitimately address.
fn session_of(ctx: &SkillContext) -> Result<u64, SkillOutcome> {
    ctx.sub.session_id.ok_or_else(|| SkillOutcome {
        ok:      false,
        summary: "background jobs are per session and this call has none".into(),
    })
}

#[async_trait]
impl Skill for JobOutput {
    fn name(&self) -> &str { JOB_OUTPUT_NAME }
    fn description(&self) -> &str {
        "Read a background job's output since your last read. Positional args: \
         <id> (e.g. `cli-3`). Ends with the job's status line."
    }
    fn positional_args(&self) -> Vec<String> { vec!["id".into()] }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let session_id = match session_of(&ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let Some(id) = args.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return fail("missing `id` arg — pass a job id like `cli-3`");
        };
        let Some((text, lost, status)) = self.0.read(session_id, id) else {
            return fail(&format!("no job `{id}` in this session"));
        };
        let mut out = String::new();
        if lost > 0 {
            out.push_str(&format!(
                "[{lost} byte(s) of earlier output were dropped — the job \
                 out-ran the {OUTPUT_CAP}-byte retention window]\n"
            ));
        }
        let window = sica_core::retain::head_tail(&text, READ_CAP * 3 / 4, READ_CAP / 4);
        out.push_str(&window.render("output"));
        if text.is_empty() {
            out.push_str("(no new output)");
        }
        out.push_str(&format!("\n[status: {}]", status.label()));
        SkillOutcome { ok: true, summary: out }
    }
}

#[async_trait]
impl Skill for JobList {
    fn name(&self) -> &str { JOB_LIST_NAME }
    fn description(&self) -> &str {
        "List this session's background jobs with their status and how much \
         output is unread. No arguments."
    }

    async fn run(&self, _args: Value, ctx: SkillContext) -> SkillOutcome {
        let session_id = match session_of(&ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let jobs = self.0.list(session_id);
        if jobs.is_empty() {
            return SkillOutcome { ok: true, summary: "no background jobs".into() };
        }
        let mut out = String::new();
        for j in &jobs {
            out.push_str(&format!(
                "{} [{}] {} — {} unread byte(s)\n",
                j.id,
                j.status.label(),
                j.command,
                j.unread
            ));
        }
        SkillOutcome { ok: true, summary: out.trim_end().to_string() }
    }
}

#[async_trait]
impl Skill for JobKill {
    fn name(&self) -> &str { JOB_KILL_NAME }
    fn description(&self) -> &str {
        "Stop a running background job. Positional args: <id> (e.g. `cli-3`). \
         The job's own process tree dies with it."
    }
    fn positional_args(&self) -> Vec<String> { vec!["id".into()] }

    async fn run(&self, args: Value, ctx: SkillContext) -> SkillOutcome {
        let session_id = match session_of(&ctx) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let Some(id) = args.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return fail("missing `id` arg — pass a job id like `cli-3`");
        };
        match self.0.kill(session_id, id) {
            Ok(()) => SkillOutcome {
                ok:      true,
                summary: format!("asked job `{id}` to stop"),
            },
            Err(e) => {
                warn!(session_id, job = %id, error = %e, "job kill refused");
                fail(&e)
            }
        }
    }
}

fn fail(msg: &str) -> SkillOutcome {
    SkillOutcome { ok: false, summary: msg.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(command: &str) -> Command {
        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", command]);
            c
        };
        c.kill_on_drop(true);
        c
    }

    async fn wait_until_done(reg: &JobRegistry, session: u64, id: &str) -> JobStatus {
        for _ in 0..200 {
            let jobs = reg.list(session);
            let job = jobs.iter().find(|j| j.id == id).expect("job listed");
            if !job.status.is_running() {
                return job.status.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("job {id} never finished");
    }

    #[tokio::test]
    async fn a_job_runs_to_completion_and_its_output_is_read_once() {
        let reg = Arc::new(JobRegistry::new());
        let id = reg.start_shell(1, "cli", "echo hello", shell("echo hello")).unwrap();
        assert_eq!(id, "cli-1", "ids are `<kind>-N`");
        let status = wait_until_done(&reg, 1, &id).await;
        assert_eq!(status, JobStatus::Exited { code: 0 });

        let (text, lost, _) = reg.read(1, &id).unwrap();
        assert!(text.contains("hello"), "read: {text:?}");
        assert_eq!(lost, 0);
        // Output is handed back once; a second read sees only what is new.
        let (again, _, _) = reg.read(1, &id).unwrap();
        assert!(again.is_empty(), "read twice: {again:?}");
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code() {
        let reg = Arc::new(JobRegistry::new());
        let id = reg.start_shell(1, "cli", "exit 3", shell("exit 3")).unwrap();
        assert_eq!(wait_until_done(&reg, 1, &id).await, JobStatus::Exited { code: 3 });
    }

    #[tokio::test]
    async fn jobs_are_invisible_to_other_sessions() {
        let reg = Arc::new(JobRegistry::new());
        let id = reg.start_shell(1, "cli", "echo x", shell("echo x")).unwrap();
        wait_until_done(&reg, 1, &id).await;
        assert!(reg.list(2).is_empty());
        assert!(reg.read(2, &id).is_none(), "another session cannot read it");
        assert!(reg.kill(2, &id).is_err(), "another session cannot kill it");
    }

    #[tokio::test]
    async fn the_per_session_limit_refuses_the_eleventh_running_job() {
        let reg = Arc::new(JobRegistry::new());
        let sleep = if cfg!(windows) {
            "ping -n 20 127.0.0.1 > NUL"
        } else {
            "sleep 20"
        };
        for _ in 0..MAX_JOBS_PER_SESSION {
            reg.start_shell(1, "cli", sleep, shell(sleep)).unwrap();
        }
        let refused = reg.start_shell(1, "cli", sleep, shell(sleep)).unwrap_err();
        assert!(refused.contains("limit"), "{refused}");
        // A different session is unaffected by another's crowd.
        assert!(reg.start_shell(2, "cli", sleep, shell(sleep)).is_ok());
        reg.clear(1);
        reg.clear(2);
    }

    #[tokio::test]
    async fn killing_a_job_settles_it_as_killed() {
        let reg = Arc::new(JobRegistry::new());
        let sleep = if cfg!(windows) {
            "ping -n 30 127.0.0.1 > NUL"
        } else {
            "sleep 30"
        };
        let id = reg.start_shell(1, "cli", sleep, shell(sleep)).unwrap();
        reg.kill(1, &id).unwrap();
        assert_eq!(wait_until_done(&reg, 1, &id).await, JobStatus::Killed);
        // Killing it twice is refused rather than silently ignored.
        assert!(reg.kill(1, &id).is_err());
    }

    #[test]
    fn output_over_the_cap_drops_the_oldest_and_the_read_says_so() {
        let mut job = Job {
            id: "cli-1".into(),
            kind: "cli".into(),
            command: "x".into(),
            status: JobStatus::Running,
            buf: Vec::new(),
            produced: 0,
            read_mark: 0,
            cancel: CancellationToken::new(),
        };
        job.append(&vec![b'a'; OUTPUT_CAP]);
        job.append(&vec![b'b'; 100]);
        let (text, lost) = job.take_unread();
        assert_eq!(lost, 100, "the oldest 100 bytes fell out of the window");
        assert_eq!(text.len(), OUTPUT_CAP);
        assert!(text.ends_with("bbbb"));
        // Nothing new since: no output, and nothing reported as lost.
        job.append(b"c");
        let (text, lost) = job.take_unread();
        assert_eq!((text.as_str(), lost), ("c", 0));
    }

    #[test]
    fn the_completion_notice_names_the_job_status_and_how_to_read_it() {
        let notice = completion_notice(&JobSummary {
            id:      "cli-2".into(),
            kind:    "cli".into(),
            command: "cargo build".into(),
            status:  JobStatus::Exited { code: 1 },
            unread:  4096,
        });
        assert!(notice.contains("cli-2"));
        assert!(notice.contains("exited 1"));
        assert!(notice.contains("cargo build"));
        assert!(notice.contains("4096 byte(s) of output are unread"));
        assert!(notice.contains(JOB_OUTPUT_NAME));
    }
}
