//! Per-session inbox: what is waiting to enter the turn loop (guide §2.1).
//!
//! Before this, a send that arrived while a turn was running cancelled that
//! turn — the only thing the loop could do with input it had no place to
//! put. The inbox gives input a place to wait, and the loop claims from it
//! at two points:
//!
//! - **At the top of every hop** it drains [`Inbound::Steer`] and
//!   [`Inbound::Inject`] into the session log, so they are part of the
//!   history the very next request derives. This is how a human redirects
//!   an agent mid-flight, and how the runtime tells it something happened
//!   (a background job finished, a goal round opened).
//! - **At the end of the turn** it claims one [`Inbound::Followup`] and
//!   runs it as the next turn, so a queued message needs no second send.
//!
//! Ordering within a session is FIFO and the queue is never reordered: a
//! steer that arrived before an inject is applied before it, because the
//! model reads them as one conversation.

use std::collections::{HashMap, VecDeque};

use protocol::UserImage;
use sica_core::event::ContextSource;
use tokio::sync::Mutex;

/// One item waiting to enter a session's loop.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A user message that arrived while a turn was running. It runs as its
    /// own turn once the current one ends — never spliced into a turn the
    /// user has not seen the end of.
    Followup { text: String, images: Vec<UserImage> },
    /// User text to splice into the *running* turn at its next hop. The
    /// user is redirecting the agent rather than waiting for it.
    Steer { text: String },
    /// Non-user context for the next hop. Model-visible, never attributed
    /// to the user: a finished background job, a goal round.
    Inject { content: String, source: ContextSource },
}

impl Inbound {
    /// The word `Event::InboxChanged.accepted` carries, so the FE can label
    /// what it just optimistically rendered.
    pub fn accepted(&self) -> &'static str {
        match self {
            Inbound::Followup { .. } => "queued",
            Inbound::Steer { .. } => "steered",
            Inbound::Inject { .. } => "injected",
        }
    }
}

/// FIFO queues, one per session.
#[derive(Default)]
pub struct Inbox {
    queues: Mutex<HashMap<u64, VecDeque<Inbound>>>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue one item. Returns how many *followups* are now waiting —
    /// the only depth worth showing, since steers and injects are consumed
    /// at the next hop rather than queued behind anything.
    pub async fn push(&self, session_id: u64, item: Inbound) -> u32 {
        let mut g = self.queues.lock().await;
        let q = g.entry(session_id).or_default();
        q.push_back(item);
        followups(q)
    }

    /// Take every steer and inject, oldest first, leaving followups where
    /// they are. Called at the top of each hop, so mid-turn input reaches
    /// the model on the next request rather than after the turn.
    pub async fn drain_mid_turn(&self, session_id: u64) -> Vec<Inbound> {
        let mut g = self.queues.lock().await;
        let Some(q) = g.get_mut(&session_id) else {
            return Vec::new();
        };
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(q.len());
        for item in q.drain(..) {
            match item {
                Inbound::Followup { .. } => kept.push_back(item),
                _ => taken.push(item),
            }
        }
        *q = kept;
        taken
    }

    /// Claim the oldest queued user message, if any. The turn loop calls
    /// this once its `TurnEnd` is written.
    pub async fn take_followup(&self, session_id: u64) -> Option<(String, Vec<UserImage>)> {
        let mut g = self.queues.lock().await;
        let q = g.get_mut(&session_id)?;
        let pos = q
            .iter()
            .position(|i| matches!(i, Inbound::Followup { .. }))?;
        match q.remove(pos) {
            Some(Inbound::Followup { text, images }) => Some((text, images)),
            _ => None,
        }
    }

    /// Followups still waiting for this session.
    pub async fn queued(&self, session_id: u64) -> u32 {
        let g = self.queues.lock().await;
        g.get(&session_id).map(followups).unwrap_or(0)
    }

    /// Drop everything for a session (it was deleted).
    pub async fn clear(&self, session_id: u64) {
        self.queues.lock().await.remove(&session_id);
    }
}

fn followups(q: &VecDeque<Inbound>) -> u32 {
    q.iter()
        .filter(|i| matches!(i, Inbound::Followup { .. }))
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn followup(text: &str) -> Inbound {
        Inbound::Followup { text: text.into(), images: Vec::new() }
    }

    #[tokio::test]
    async fn followups_queue_and_are_claimed_oldest_first() {
        let inbox = Inbox::new();
        assert_eq!(inbox.push(1, followup("first")).await, 1);
        assert_eq!(inbox.push(1, followup("second")).await, 2);
        assert_eq!(inbox.take_followup(1).await.unwrap().0, "first");
        assert_eq!(inbox.queued(1).await, 1);
        assert_eq!(inbox.take_followup(1).await.unwrap().0, "second");
        assert!(inbox.take_followup(1).await.is_none());
    }

    #[tokio::test]
    async fn mid_turn_drain_takes_steers_and_injects_in_order_and_keeps_followups() {
        let inbox = Inbox::new();
        inbox.push(1, Inbound::Steer { text: "actually, do X".into() }).await;
        inbox.push(1, followup("later question")).await;
        inbox.push(
            1,
            Inbound::Inject {
                content: "job cli-1 finished".into(),
                source: ContextSource::ToolNotice,
            },
        )
        .await;

        let taken = inbox.drain_mid_turn(1).await;
        assert_eq!(taken.len(), 2);
        assert!(matches!(taken[0], Inbound::Steer { .. }), "FIFO across kinds");
        assert!(matches!(taken[1], Inbound::Inject { .. }));
        // The queued user message is still waiting for its own turn.
        assert_eq!(inbox.queued(1).await, 1);
        assert!(inbox.drain_mid_turn(1).await.is_empty(), "drained items do not repeat");
    }

    #[tokio::test]
    async fn queues_are_per_session() {
        let inbox = Inbox::new();
        inbox.push(1, followup("a")).await;
        assert_eq!(inbox.queued(2).await, 0);
        assert!(inbox.take_followup(2).await.is_none());
        assert!(inbox.drain_mid_turn(2).await.is_empty());
        inbox.clear(1).await;
        assert_eq!(inbox.queued(1).await, 0);
    }

    #[test]
    fn accepted_names_the_kind_for_the_frontend() {
        assert_eq!(followup("x").accepted(), "queued");
        assert_eq!(Inbound::Steer { text: "x".into() }.accepted(), "steered");
        assert_eq!(
            Inbound::Inject {
                content: "x".into(),
                source: ContextSource::ToolNotice
            }
            .accepted(),
            "injected"
        );
    }
}
