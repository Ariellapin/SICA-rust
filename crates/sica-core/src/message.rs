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
    /// OpenAI-native `tool_calls` array (JSON text) on assistant messages
    /// produced in native tool-calling mode. Replayed verbatim on the wire
    /// so the server's chat template sees the original call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<String>,
    /// Correlation id on `Tool`-role messages in native mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    fn base(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: text.into(),
            reasoning: None,
            images: Vec::new(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::base(Role::User, text)
    }

    pub fn user_with_images(text: impl Into<String>, images: Vec<UserImage>) -> Self {
        Self { images, ..Self::base(Role::User, text) }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::base(Role::Assistant, text)
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::base(Role::System, text)
    }
}
