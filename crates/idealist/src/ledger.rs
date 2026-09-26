//! Per-session ledger: which tickets a session raised, and whether the
//! session has been investigated since.
//!
//! `idealist_workspace/sessions/<session_id>.toml`. Written at the point of
//! failure (so it exists even when the daemon is behind), updated at each
//! `TurnEnd` with whether each tool failure was recovered from, and read at
//! session end by the investigator.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::trigger_bus::TriggerOrigin;

static WRITE: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub ticket_id: String,
    pub origin:    TriggerOrigin,
    /// The failing skill, for tool-call tickets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill:     Option<String>,
    /// Seq of the `TicketOpened` row — where the investigator looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq:       Option<u64>,
    /// Newest seq this ticket fired at in this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq:  Option<u64>,
    /// Times it fired in this session.
    pub count:     u32,
    /// Turns where a later call of the same skill succeeded / did not.
    #[serde(default)]
    pub recovered_turns:   u32,
    #[serde(default)]
    pub unrecovered_turns: u32,
    #[serde(default)]
    pub investigated:      bool,
}

impl LedgerEntry {
    /// Every turn it failed in, the agent went on to make the same skill
    /// work. Normal behaviour — a wrong path followed by the right one — not
    /// something to spend an investigation on unless it keeps happening.
    pub fn recovered(&self) -> bool {
        self.skill.is_some() && self.recovered_turns > 0 && self.unrecovered_turns == 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionLedger {
    pub session_id:      u64,
    #[serde(default)]
    pub entries:         Vec<LedgerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub investigated_at: Option<String>,
}

impl SessionLedger {
    /// Entries the investigator has not looked at yet.
    pub fn pending(&self) -> impl Iterator<Item = &LedgerEntry> {
        self.entries.iter().filter(|e| !e.investigated)
    }

    pub fn has_pending(&self) -> bool {
        self.pending().next().is_some()
    }
}

/// What [`Ledger::record`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The ticket's first failure in this session — log a `TicketOpened`.
    First,
    /// Already in this session's ledger; the count went up.
    Repeat,
}

#[derive(Debug, Clone)]
pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    /// `<idealist_workspace>/sessions`.
    pub fn open_default() -> Self {
        Self::at(sica_core::paths::idealist_workspace().join("sessions"))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, session_id: u64) -> PathBuf {
        self.dir.join(format!("{session_id}.toml"))
    }

    /// The session's ledger; an empty one when there is none yet.
    pub fn load(&self, session_id: u64) -> SessionLedger {
        fs::read_to_string(self.path(session_id))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or(SessionLedger { session_id, ..Default::default() })
    }

    fn save(&self, l: &SessionLedger) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(l.session_id);
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string(l).context("serialise ledger")?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn update<R>(&self, session_id: u64, f: impl FnOnce(&mut SessionLedger) -> R) -> Result<R> {
        let _g = WRITE.lock().unwrap_or_else(|p| p.into_inner());
        let mut l = self.load(session_id);
        let r = f(&mut l);
        self.save(&l)?;
        Ok(r)
    }

    /// Would [`Self::record`] be this ticket's first failure in the session?
    /// Lets the caller decide whether to log a `TicketOpened` *before* it
    /// knows the row's seq.
    pub fn is_new(&self, session_id: u64, ticket_id: &str) -> bool {
        !self.load(session_id).entries.iter().any(|e| e.ticket_id == ticket_id)
    }

    /// Note one failure. A repeat un-marks the entry as investigated: it
    /// happened again after the investigator looked, so it is news.
    pub fn record(
        &self,
        session_id: u64,
        ticket_id: &str,
        origin: TriggerOrigin,
        skill: Option<&str>,
        seq: Option<u64>,
    ) -> Result<Recorded> {
        self.update(session_id, |l| {
            if let Some(e) = l.entries.iter_mut().find(|e| e.ticket_id == ticket_id) {
                e.count = e.count.saturating_add(1);
                if seq.is_some() {
                    e.last_seq = seq;
                }
                e.investigated = false;
                return Recorded::Repeat;
            }
            l.entries.push(LedgerEntry {
                ticket_id: ticket_id.to_string(),
                origin,
                skill: skill.map(str::to_string),
                seq,
                last_seq: seq,
                count: 1,
                recovered_turns: 0,
                unrecovered_turns: 0,
                investigated: false,
            });
            Recorded::First
        })
    }

    /// Fold one finished turn in: for each skill that failed during it,
    /// whether a later call of the same skill succeeded. Applies to entries
    /// that fired at or after `since_seq` (the turn's start).
    pub fn apply_turn(
        &self,
        session_id: u64,
        since_seq: u64,
        outcome: &HashMap<String, bool>,
    ) -> Result<()> {
        if outcome.is_empty() || !self.path(session_id).exists() {
            return Ok(());
        }
        self.update(session_id, |l| {
            for e in &mut l.entries {
                let (Some(skill), Some(last)) = (e.skill.as_deref(), e.last_seq) else {
                    continue;
                };
                if last < since_seq {
                    continue;
                }
                match outcome.get(skill) {
                    Some(true) => e.recovered_turns = e.recovered_turns.saturating_add(1),
                    Some(false) => e.unrecovered_turns = e.unrecovered_turns.saturating_add(1),
                    None => {}
                }
            }
        })
    }

    /// Mark every pending entry investigated (or deliberately skipped).
    pub fn mark_investigated(&self, session_id: u64) -> Result<()> {
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        self.update(session_id, |l| {
            for e in &mut l.entries {
                e.investigated = true;
            }
            l.investigated_at = Some(stamp);
        })
    }

    /// Sessions with entries nobody has investigated — the startup sweep's
    /// queue. Ascending by id, so older sessions go first.
    pub fn pending_sessions(&self) -> Vec<u64> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != "toml" {
                    return None;
                }
                p.file_stem()?.to_str()?.parse().ok()
            })
            .filter(|id| self.load(*id).has_pending())
            .collect();
        ids.sort_unstable();
        ids
    }
}

/// Which skills failed in a turn, and whether each was recovered from:
/// `true` when a later call of the same skill in the same turn succeeded.
/// `results` is the turn's tool results in log order, `(skill, ok)`.
pub fn turn_recovery<'a>(results: impl IntoIterator<Item = (&'a str, bool)>) -> HashMap<String, bool> {
    let mut out: HashMap<String, bool> = HashMap::new();
    for (skill, ok) in results {
        if ok {
            if let Some(v) = out.get_mut(skill) {
                *v = true;
            }
        } else {
            // A failure after a success re-opens the question for the rest
            // of the turn: the *last* word on a skill is what counts.
            out.insert(skill.to_string(), false);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(tag: &str) -> Ledger {
        Ledger::at(crate::ticket::tests::scratch(tag))
    }

    #[test]
    fn first_then_repeat() {
        let l = ledger("ledger-first");
        assert!(l.is_new(4, "abc"));
        let r1 = l.record(4, "abc", TriggerOrigin::ToolCall, Some("glob"), Some(10)).unwrap();
        let r2 = l.record(4, "abc", TriggerOrigin::ToolCall, Some("glob"), Some(15)).unwrap();
        assert_eq!((r1, r2), (Recorded::First, Recorded::Repeat));
        let s = l.load(4);
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].count, 2);
        assert_eq!(s.entries[0].seq, Some(10));
        assert_eq!(s.entries[0].last_seq, Some(15));
        assert!(!l.is_new(4, "abc"));
    }

    #[test]
    fn recovery_is_the_last_word_per_skill() {
        let r = turn_recovery([("read-file", false), ("read-file", true), ("glob", false)]);
        assert_eq!(r.get("read-file"), Some(&true));
        assert_eq!(r.get("glob"), Some(&false));
        let r = turn_recovery([("read-file", false), ("read-file", true), ("read-file", false)]);
        assert_eq!(r.get("read-file"), Some(&false));
        assert!(turn_recovery([("glob", true)]).is_empty());
    }

    #[test]
    fn apply_turn_marks_recovered_entries() {
        let l = ledger("ledger-recover");
        l.record(1, "t1", TriggerOrigin::ToolCall, Some("read-file"), Some(5)).unwrap();
        l.record(1, "t2", TriggerOrigin::ToolCall, Some("glob"), Some(6)).unwrap();
        l.record(1, "t3", TriggerOrigin::TurnError, None, Some(7)).unwrap();
        let outcome = turn_recovery([("read-file", false), ("read-file", true), ("glob", false)]);
        l.apply_turn(1, 3, &outcome).unwrap();
        let s = l.load(1);
        let by = |id: &str| s.entries.iter().find(|e| e.ticket_id == id).unwrap().clone();
        assert!(by("t1").recovered());
        assert!(!by("t2").recovered());
        assert!(!by("t3").recovered(), "a turn error is never 'recovered'");
        // Entries from before the turn are untouched.
        l.apply_turn(1, 100, &outcome).unwrap();
        assert_eq!(l.load(1).entries[0].recovered_turns, 1);
    }

    #[test]
    fn investigated_sessions_leave_the_queue_until_a_repeat() {
        let l = ledger("ledger-pending");
        l.record(9, "a", TriggerOrigin::TurnError, None, None).unwrap();
        l.record(11, "b", TriggerOrigin::TurnError, None, None).unwrap();
        assert_eq!(l.pending_sessions(), vec![9, 11]);
        l.mark_investigated(9).unwrap();
        assert_eq!(l.pending_sessions(), vec![11]);
        assert!(l.load(9).investigated_at.is_some());
        l.record(9, "a", TriggerOrigin::TurnError, None, None).unwrap();
        assert_eq!(l.pending_sessions(), vec![9, 11]);
    }
}
