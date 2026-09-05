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
use std::sync::atomic::{AtomicU64, Ordering};

use protocol::{QueuedDump, UserImage};
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

/// A queued item and the handle the frontend addresses it by.
///
/// The id is minted here rather than derived from a position, because a
/// position stops naming the same row the moment the loop claims one: an
/// "edit row 0" racing a claim would rewrite the wrong message.
#[derive(Debug, Clone)]
struct Queued {
    id:   u64,
    item: Inbound,
}

/// FIFO queues, one per session.
#[derive(Default)]
pub struct Inbox {
    queues:  Mutex<HashMap<u64, VecDeque<Queued>>>,
    /// Monotonic across every session — ids only have to be unique, and one
    /// counter is simpler to reason about than one per session.
    next_id: AtomicU64,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue one item. Returns the followups now waiting, in the order
    /// they will run — the only depth worth showing, since steers and
    /// injects are consumed at the next hop rather than queued behind
    /// anything. The count the frontend labels with is `rows.len()`.
    pub async fn push(&self, session_id: u64, item: Inbound) -> Vec<QueuedDump> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut g = self.queues.lock().await;
        let q = g.entry(session_id).or_default();
        q.push_back(Queued { id, item });
        dump(q)
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
        for entry in q.drain(..) {
            match entry.item {
                Inbound::Followup { .. } => kept.push_back(entry),
                item => taken.push(item),
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
            .position(|e| matches!(e.item, Inbound::Followup { .. }))?;
        match q.remove(pos).map(|e| e.item) {
            Some(Inbound::Followup { text, images }) => Some((text, images)),
            _ => None,
        }
    }

    /// Rewrite a waiting followup. `false` means the id named nothing — the
    /// loop claimed it first, or it was already removed.
    pub async fn edit(&self, session_id: u64, id: u64, text: String) -> bool {
        let mut g = self.queues.lock().await;
        let Some(q) = g.get_mut(&session_id) else { return false };
        match q.iter_mut().find(|e| e.id == id).map(|e| &mut e.item) {
            Some(Inbound::Followup { text: slot, .. }) => {
                *slot = text;
                true
            }
            _ => false,
        }
    }

    /// Drop a waiting followup and hand it back, so a caller that is moving
    /// it somewhere else (a steer) never has to read it first and race the
    /// claim between the two calls.
    pub async fn remove(&self, session_id: u64, id: u64) -> Option<Inbound> {
        let mut g = self.queues.lock().await;
        let q = g.get_mut(&session_id)?;
        let pos = q
            .iter()
            .position(|e| e.id == id && matches!(e.item, Inbound::Followup { .. }))?;
        q.remove(pos).map(|e| e.item)
    }

    /// The followups waiting for this session, in run order.
    pub async fn rows(&self, session_id: u64) -> Vec<QueuedDump> {
        let g = self.queues.lock().await;
        g.get(&session_id).map(dump).unwrap_or_default()
    }

    /// Followups still waiting for this session.
    pub async fn queued(&self, session_id: u64) -> u32 {
        let g = self.queues.lock().await;
        g.get(&session_id).map(|q| dump(q).len() as u32).unwrap_or(0)
    }

    /// Drop everything for a session (it was deleted).
    pub async fn clear(&self, session_id: u64) {
        self.queues.lock().await.remove(&session_id);
    }
}

/// The wire view of a queue: followups only. A steer or inject is spent at
/// the running turn's next hop, so it is never something the user could
/// meaningfully edit or remove.
fn dump(q: &VecDeque<Queued>) -> Vec<QueuedDump> {
    q.iter()
        .filter_map(|e| match &e.item {
            Inbound::Followup { text, images } => Some(QueuedDump {
                id:     e.id,
                text:   text.clone(),
                images: images.len() as u32,
            }),
            _ => None,
        })
        .collect()
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
        assert_eq!(inbox.push(1, followup("first")).await.len(), 1);
        assert_eq!(inbox.push(1, followup("second")).await.len(), 2);
        assert_eq!(inbox.take_followup(1).await.unwrap().0, "first");
        assert_eq!(inbox.queued(1).await, 1);
        assert_eq!(inbox.take_followup(1).await.unwrap().0, "second");
        assert!(inbox.take_followup(1).await.is_none());
    }

    /// The dock addresses rows by id, so an id has to keep naming the same
    /// message after the row in front of it is claimed — the whole reason
    /// positions are not the handle.
    #[tokio::test]
    async fn ids_survive_a_claim_in_front_of_them() {
        let inbox = Inbox::new();
        inbox.push(1, followup("first")).await;
        let rows = inbox.push(1, followup("second")).await;
        let second = rows[1].id;

        inbox.take_followup(1).await.unwrap();
        assert!(inbox.edit(1, second, "second, edited".into()).await);
        assert_eq!(inbox.rows(1).await[0].text, "second, edited");
        assert_eq!(inbox.take_followup(1).await.unwrap().0, "second, edited");
    }

    #[tokio::test]
    async fn edit_and_remove_report_a_row_that_is_gone() {
        let inbox = Inbox::new();
        let id = inbox.push(1, followup("only")).await[0].id;
        assert!(inbox.remove(1, id).await.is_some());
        assert!(inbox.rows(1).await.is_empty());
        // Claimed, removed and never-real all look the same from here, and
        // all three have to report failure rather than silently no-op.
        assert!(!inbox.edit(1, id, "too late".into()).await);
        assert!(inbox.remove(1, id).await.is_none());
        assert!(!inbox.edit(1, 999, "no such row".into()).await);
    }

    /// Steers and injects are spent at the next hop, so they are not rows
    /// the dock can show or the user can address.
    #[tokio::test]
    async fn only_followups_are_addressable_rows() {
        let inbox = Inbox::new();
        inbox.push(1, Inbound::Steer { text: "redirect".into() }).await;
        let rows = inbox.push(1, followup("wait for me")).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "wait for me");
        // The steer is still there to be drained; it just is not a row.
        assert_eq!(inbox.drain_mid_turn(1).await.len(), 1);
    }

    #[tokio::test]
    async fn rows_report_the_images_riding_with_a_message() {
        let inbox = Inbox::new();
        let rows = inbox
            .push(1, Inbound::Followup {
                text:   "look at these".into(),
                images: vec![
                    UserImage { mime: "image/png".into(), data_base64: "a".into(), ..Default::default() },
                    UserImage { mime: "image/png".into(), data_base64: "b".into(), ..Default::default() },
                ],
            })
            .await;
        assert_eq!(rows[0].images, 2);
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
