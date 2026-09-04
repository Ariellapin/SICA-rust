//! Bridge from `agents::jobs` back into the session (guide §12.4).
//!
//! The `agents` crate owns the job registry but knows nothing about session
//! logs, inboxes or the wire — it only knows how to call a
//! [`JobNotifier`]. This is that notifier: a finished job becomes a durable
//! `JobFinished` audit line, a `ContextInjected { source: JobNotice }` in
//! the session's inbox, and a pushed `JobsChanged` for the frontend.
//!
//! Delivery is deliberately pull-free but *not* autonomous. A running turn
//! picks the notice up at its next hop; an idle session picks it up at the
//! start of its next turn. Nothing here wakes an idle agent to start a turn
//! of its own — that is the goal driver's job (§12.3), where autonomous
//! continuation is the whole point and is bounded by a round cap. A build
//! finishing is not, by itself, a reason to spend a turn.

use std::sync::Arc;

use agents::jobs::{completion_notice, JobNotifier, JobRegistry, JobSummary};
use agents::EventSink;
use protocol::{Event, JobDump};
use sica_core::event::{ContextSource, EventKind};

use crate::chat::{append_event, ChatHub, Sessions};
use crate::inbox::{Inbound, Inbox};

pub struct JobsBridge {
    sessions: Sessions,
    inbox:    Arc<Inbox>,
    events:   Arc<dyn EventSink>,
    jobs:     Arc<JobRegistry>,
}

impl JobsBridge {
    pub fn new(hub: &ChatHub) -> Self {
        Self {
            sessions: hub.sessions.clone(),
            inbox:    hub.inbox.clone(),
            events:   hub.event_sink.clone(),
            jobs:     hub.jobs.clone(),
        }
    }

    fn dump(&self, session_id: u64) -> Vec<JobDump> {
        self.jobs
            .list(session_id)
            .into_iter()
            .map(|j| JobDump {
                id:      j.id,
                kind:    j.kind,
                command: j.command,
                status:  j.status.label(),
                running: j.status.is_running(),
                unread:  j.unread,
            })
            .collect()
    }
}

impl JobNotifier for JobsBridge {
    fn finished(&self, session_id: u64, job: &JobSummary) {
        let notice = completion_notice(job);
        let audit = EventKind::JobFinished {
            id:        job.id.clone(),
            status:    job.status.label(),
            exit_code: job.status.exit_code(),
        };
        let label = job.status.label();
        let id = job.id.clone();
        let sessions = self.sessions.clone();
        let inbox = self.inbox.clone();
        let events = self.events.clone();
        // The notifier is called from the job's watcher task and must not
        // block it; the log append and the inbox push are both async.
        tokio::spawn(async move {
            append_event(&sessions, session_id, audit).await;
            inbox
                .push(
                    session_id,
                    Inbound::Inject { content: notice, source: ContextSource::JobNotice },
                )
                .await;
            events.emit(Event::LogLine {
                level:   "INFO".into(),
                message: format!("job {id} {label}"),
            });
        });
    }

    fn changed(&self, session_id: u64) {
        self.events.emit(Event::JobsChanged {
            session_id,
            jobs: self.dump(session_id),
        });
    }
}
