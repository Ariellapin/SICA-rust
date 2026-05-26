use serde::{Deserialize, Serialize};

use protocol::UserImage;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub reasoning: Option<String>,
    /// Image attachments (only meaningful on user messages today). `#[serde(default)]`
    /// keeps sessions saved before vision support readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<UserImage>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: text.into(), reasoning: None, images: Vec::new() }
    }

    pub fn user_with_images(text: impl Into<String>, images: Vec<UserImage>) -> Self {
        Self { role: Role::User, content: text.into(), reasoning: None, images }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: text.into(), reasoning: None, images: Vec::new() }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self { role: Role::System, content: text.into(), reasoning: None, images: Vec::new() }
    }
}
