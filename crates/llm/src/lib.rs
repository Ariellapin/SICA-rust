//! LLM HTTP client (llama.cpp-compatible OpenAI API), streaming, tokenize, state machine.

pub mod client;
pub mod state;
pub mod streaming;
pub mod tokenize;

pub use client::{ChatRequest, LlmClient, StreamChunk};
pub use state::{LlmConnection, LlmEvent};

pub use protocol::LlmState;
