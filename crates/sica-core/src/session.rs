use serde::{Deserialize, Serialize};

use crate::message::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: u64,
    pub title: String,
    pub created_at: i64,
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new(id: u64, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            created_at: chrono::Utc::now().timestamp(),
            messages: Vec::new(),
        }
    }
}
