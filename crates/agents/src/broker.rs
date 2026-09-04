//! Human-in-the-loop brokers (Wave 3, guide §10.1–§10.2).
//!
//! Two one-shot rendezvous over the pipe:
//!
//! - **Approvals.** A pipeline `Ask` emits `Event::ApprovalRequested`; the
//!   FE answers Allow-once / Deny via `Request::ResolveApproval`. Missing,
//!   late, or timed-out answers fail closed to *deny*.
//! - **Questions.** The `ask-user` skill (and plan review) emits
//!   `Event::QuestionAsked`; the FE answers via `Request::AnswerQuestion`.
//!   A timeout or interrupt resolves to `None` and the skill fails loudly
//!   instead of hanging the turn.
//!
//! One `BrokerSet` lives on `ChatHub`; sub-agents hold an `Arc` to it.
//! Runtime-owned children (teammates) never get one — a teammate that needs
//! a human must put the unresolved question in its final report.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use protocol::Event;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::EventSink;

/// One-shot policy decisions time out to deny after this long.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Human questions time out after this long. Must stay under the asking
/// skill's own `timeout()` or the skill dies first.
pub const QUESTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
pub struct BrokerSet {
    next: AtomicU64,
    approvals: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    questions: Mutex<HashMap<u64, oneshot::Sender<String>>>,
}

impl BrokerSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn next_id(&self) -> u64 {
        // 0 is reserved for unsolicited events; start ids at 1.
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Ask the human to allow one tool call. Returns `true` only on an
    /// explicit Allow; timeouts, interrupts and unknown ids are deny.
    pub async fn ask_approval(
        &self,
        emit: &Arc<dyn EventSink>,
        session_id: u64,
        skill: &str,
        args_preview: &str,
        reason: &str,
        cancel: Option<CancellationToken>,
    ) -> bool {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.approvals.lock().await.insert(id, tx);
        emit.emit(Event::ApprovalRequested {
            id,
            session_id,
            skill: skill.to_string(),
            args_preview: args_preview.to_string(),
            reason: reason.to_string(),
        });
        let verdict = wait(rx, APPROVAL_TIMEOUT, cancel).await.unwrap_or(false);
        self.approvals.lock().await.remove(&id);
        verdict
    }

    /// Ask the human a question with suggested options. The FE may answer
    /// with an option or free text. `None` on timeout / interrupt.
    pub async fn ask_question(
        &self,
        emit: &Arc<dyn EventSink>,
        session_id: u64,
        question: &str,
        options: &[String],
        cancel: Option<CancellationToken>,
    ) -> Option<String> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.questions.lock().await.insert(id, tx);
        emit.emit(Event::QuestionAsked {
            id,
            session_id,
            question: question.to_string(),
            options: options.to_vec(),
        });
        let answer = wait(rx, QUESTION_TIMEOUT, cancel).await;
        self.questions.lock().await.remove(&id);
        answer
    }

    /// Deliver an approval verdict. Returns whether the id was pending — a
    /// late answer (after timeout) reports `false` so the dispatcher can
    /// say so instead of pretending it applied.
    ///
    /// Awaits the map lock rather than trying it: a `try_lock` that loses a
    /// race with a concurrent `ask_*` would report the human's click as
    /// "no longer pending" and leave the call hanging until its timeout.
    pub async fn resolve_approval(&self, id: u64, allow: bool) -> bool {
        match self.approvals.lock().await.remove(&id) {
            Some(tx) => {
                let _ = tx.send(allow);
                true
            }
            None => false,
        }
    }

    /// Deliver a question answer. Same pending-or-late contract.
    pub async fn answer_question(&self, id: u64, answer: String) -> bool {
        match self.questions.lock().await.remove(&id) {
            Some(tx) => {
                let _ = tx.send(answer);
                true
            }
            None => false,
        }
    }

    #[cfg(test)]
    async fn pending_approvals(&self) -> usize {
        self.approvals.lock().await.len()
    }

    #[cfg(test)]
    async fn pending_questions(&self) -> usize {
        self.questions.lock().await.len()
    }
}

/// Wait for a one-shot answer with a timeout and an interrupt. `Err` on a
/// dropped sender is treated like a timeout — fail closed / resolve none.
async fn wait<T>(
    rx: oneshot::Receiver<T>,
    timeout: Duration,
    cancel: Option<CancellationToken>,
) -> Option<T> {
    match cancel {
        Some(tok) => tokio::select! {
            biased;
            _ = tok.cancelled() => None,
            res = tokio::time::timeout(timeout, rx) => res.ok().and_then(|r| r.ok()),
        },
        None => tokio::time::timeout(timeout, rx).await.ok().and_then(|r| r.ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct Capture(StdMutex<Vec<Event>>);
    impl EventSink for Capture {
        fn emit(&self, ev: Event) {
            self.0.lock().unwrap().push(ev);
        }
    }

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(Capture(StdMutex::new(Vec::new())))
    }

    #[tokio::test]
    async fn approval_round_trip() {
        let b = BrokerSet::new();
        let emit = sink();
        let (verdict, _) = tokio::join!(
            b.ask_approval(&emit, 7, "run-cli", "run-cli 'x'", "why", None),
            async {
                for _ in 0..100 {
                    if b.pending_approvals().await == 1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert!(b.resolve_approval(1, true).await);
            }
        );
        assert!(verdict);
        assert_eq!(b.pending_approvals().await, 0);
    }

    #[tokio::test]
    async fn late_approval_is_not_pending() {
        let b = BrokerSet::new();
        assert!(!b.resolve_approval(999, true).await);
        assert!(!b.answer_question(999, "x".into()).await);
    }

    #[tokio::test]
    async fn cancelled_approval_fails_closed() {
        let b = BrokerSet::new();
        let emit = sink();
        let tok = CancellationToken::new();
        tok.cancel();
        let verdict = b.ask_approval(&emit, 7, "s", "p", "r", Some(tok)).await;
        assert!(!verdict);
        assert_eq!(b.pending_approvals().await, 0);
    }

    #[tokio::test]
    async fn question_round_trip() {
        let b = BrokerSet::new();
        let emit = sink();
        let options = ["a".to_string(), "b".to_string()];
        let (answer, _) = tokio::join!(
            b.ask_question(&emit, 7, "which?", &options, None),
            async {
                for _ in 0..100 {
                    if b.pending_questions().await == 1 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert!(b.answer_question(1, "b".into()).await);
            }
        );
        assert_eq!(answer.as_deref(), Some("b"));
        assert_eq!(b.pending_questions().await, 0);
    }

    #[tokio::test]
    async fn cancelled_question_resolves_none() {
        let b = BrokerSet::new();
        let emit = sink();
        let tok = CancellationToken::new();
        tok.cancel();
        let ans = b.ask_question(&emit, 7, "q?", &[], Some(tok)).await;
        assert!(ans.is_none());
    }
}
