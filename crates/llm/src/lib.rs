//! LLM HTTP client (llama.cpp-compatible OpenAI API), streaming, tokenize, state machine.

pub mod client;
pub mod state;
pub mod streaming;
pub mod tokenize;

pub use client::{ChatContent, ChatMessage, ChatRequest, ContentPart, ImageUrl, LlmClient, StreamChunk};
pub use state::{LlmConnection, LlmEvent};

pub use protocol::LlmState;
