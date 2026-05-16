//! `ToolSubAgent`: wraps one tool invocation and emits start/finish events.
//! Each sub-agent carries its own depth + parent id so nested calls produce a
//! traceable chain — fixing the original Python limitation where sub-agents
//! could not invoke further sub-agents.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use once_cell::sync::Lazy;
use protocol::Event;
use serde_json::Value;

use crate::agent::EventSink;
use crate::skill::{Skill, SkillContext, SkillOutcome};

static TOOL_ID: Lazy<AtomicU64> = Lazy::new(|| AtomicU64::new(1));

fn next_tool_id() -> u64 {
    TOOL_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
pub struct ToolSubAgent {
    pub depth:     u8,
    pub parent_id: Option<u64>,
    pub max_depth: u8,
    pub events:    Arc<dyn EventSink>,
}

impl ToolSubAgent {
    pub fn root(events: Arc<dyn EventSink>) -> Self {
        Self { depth: 0, parent_id: None, max_depth: 4, events }
    }

    /// Build a child sub-agent rooted at the call id `parent_id`. Used by
    /// `SkillContext` so a skill can spawn further sub-agents.
    pub fn child(&self, parent_id: u64) -> Self {
        Self {
            depth:     self.depth.saturating_add(1),
            parent_id: Some(parent_id),
            max_depth: self.max_depth,
            events:    self.events.clone(),
        }
    }

    /// Run one skill invocation; emit ToolCallStarted/Finished around it.
    pub async fn run(&self, skill: &dyn Skill, args: Value) -> SkillOutcome {
        if self.depth >= self.max_depth {
            return SkillOutcome {
                ok: false,
                summary: format!("sub-agent depth limit ({}) reached", self.max_depth),
            };
        }

        let id = next_tool_id();
        self.events.emit(Event::ToolCallStarted {
            id,
            parent_id: self.parent_id,
            depth:     self.depth,
            name:      skill.name().to_string(),
        });

        let ctx = SkillContext { sub: self.child(id) };
        let outcome = skill.run(args, ctx).await;

        self.events.emit(Event::ToolCallFinished {
            id,
            ok: outcome.ok,
            summary: outcome.summary.clone(),
        });
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct Capture(Mutex<Vec<Event>>);

    impl EventSink for Capture {
        fn emit(&self, ev: Event) {
            self.0.lock().unwrap().push(ev);
        }
    }

    struct Echo;
    #[async_trait]
    impl Skill for Echo {
        fn name(&self) -> &str { "echo" }
        async fn run(&self, _args: Value, _ctx: SkillContext) -> SkillOutcome {
            SkillOutcome { ok: true, summary: "echoed".into() }
        }
    }

    #[tokio::test]
    async fn child_increments_depth_and_parent() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let root = ToolSubAgent::root(cap.clone());
        let outcome = root.run(&Echo, Value::Null).await;
        assert!(outcome.ok);
        let events = cap.0.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        if let Event::ToolCallStarted { depth, parent_id, .. } = &events[0] {
            assert_eq!(*depth, 0);
            assert!(parent_id.is_none());
        } else { panic!("expected ToolCallStarted"); }
    }

    #[tokio::test]
    async fn depth_limit_aborts() {
        let cap = Arc::new(Capture(Mutex::new(Vec::new())));
        let mut sub = ToolSubAgent::root(cap.clone());
        sub.depth = sub.max_depth;
        let outcome = sub.run(&Echo, Value::Null).await;
        assert!(!outcome.ok);
        assert!(outcome.summary.contains("depth limit"));
    }
}
