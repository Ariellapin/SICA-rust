//! LLM HTTP client (llama.cpp-compatible OpenAI API), streaming, tokenize, state machine.

pub mod client;
/// A scripted fault server for tests (guide §14.2). Compiled for this
/// crate's own tests, and behind the `mock` feature for anyone else's.
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod preset;
pub mod replay;
pub mod retry;
pub mod state;
pub mod streaming;
pub mod tokenize;

pub use client::{ChatContent, ChatMessage, ChatRequest, ContentPart, ImageUrl, LlmClient, StreamChunk};
pub use preset::{preset_for_model, preset_for_provider, ModelPreset};
pub use state::{LlmConnection, LlmEvent};

pub use protocol::LlmState;
