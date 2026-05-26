//! HTTP client for an OpenAI-compatible /v1/chat/completions endpoint (llama.cpp).

use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use eventsource_stream::Eventsource;

use crate::streaming::{parse_sse_event, ThinkSplitter};
pub use crate::streaming::StreamChunk;

#[derive(Clone, Debug)]
pub struct LlmClient {
    pub base_url: String,
    pub model:    String,
    api_key:      Option<String>,
    http:         reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model:    String,
    pub messages: Vec<ChatMessage>,
    pub stream:   bool,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role:    String,
    pub content: ChatContent,
}

/// OpenAI / vLLM multimodal content field: either a bare string or an array
/// of typed parts (`{type: "text", ...}` / `{type: "image_url", ...}`).
/// `#[serde(untagged)]` makes it serialize transparently so vision-capable
/// servers accept image attachments and text-only servers see plain strings.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ChatContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

impl ChatContent {
    /// Concatenation of the textual parts — used for token-count heuristics.
    /// Image parts contribute nothing to the count; their cost is opaque to us.
    pub fn text(&self) -> String {
        match self {
            ChatContent::Text(s) => s.clone(),
            ChatContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::ImageUrl { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

impl From<String> for ChatContent {
    fn from(s: String) -> Self {
        ChatContent::Text(s)
    }
}

impl From<&str> for ChatContent {
    fn from(s: &str) -> Self {
        ChatContent::Text(s.to_string())
    }
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: ChatContent::Text(content.into()) }
    }
}

#[derive(Debug, Deserialize)]
struct ModelsList {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

impl LlmClient {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model:    model.into(),
            api_key:  api_key.filter(|k| !k.is_empty()),
            http:     reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None    => req,
        }
    }

    /// GET /health — checks the server is reachable. Best-effort; some llama.cpp
    /// builds don't expose /health, in which case we fall back to GET /v1/models.
    pub async fn health(&self) -> Result<()> {
        let url = format!("{}/health", self.base_url.trim_end_matches('/'));
        // Treat any non-error transport response as reachable. For providers
        // that don't expose /health (or return 404), fall back to /v1/models
        // which also validates the API key when one is present.
        if let Ok(resp) = self.auth(self.http.get(url)).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        let _ = self.list_models().await?;
        Ok(())
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/v1/models", self.base_url.trim_end_matches('/'));
        let resp = self
            .auth(self.http.get(url))
            .send()
            .await?
            .error_for_status()?;
        let list: ModelsList = resp.json().await?;
        Ok(list.data.into_iter().map(|m| m.id).collect())
    }

    /// One-shot, non-streaming chat: aggregates every streamed delta into a
    /// single trimmed string. Used by the sub-agent summarizer where the
    /// caller doesn't need the live deltas surfaced to the UI.
    pub async fn chat_once(&self, messages: Vec<ChatMessage>) -> Result<String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let this = self.clone();
        let handle = tokio::spawn(async move {
            this.chat_stream(messages, tx, None).await
        });
        let mut out = String::new();
        while let Some(chunk) = rx.recv().await {
            out.push_str(&chunk.delta_content);
        }
        match handle.await {
            Ok(Ok(())) => Ok(out.trim().to_string()),
            Ok(Err(e)) => Err(e),
            Err(e)     => Err(anyhow!("join: {e}")),
        }
    }

    /// Open a streaming chat completion. Each parsed `StreamChunk` is forwarded
    /// to `tx`. The task returns once the upstream stream closes, errors, or
    /// `cancel` fires (used by `InterruptTurn` to stop generation mid-flight).
    pub async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tx: mpsc::UnboundedSender<StreamChunk>,
        cancel: Option<CancellationToken>,
    ) -> Result<()> {
        let url = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));
        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: true,
            temperature: Some(0.7),
        };

        // POST itself is racy against cancellation: if Esc fires before the
        // server starts streaming, bail without consuming the body.
        let send_fut = self.auth(self.http.post(url).json(&body)).send();
        let resp = match &cancel {
            Some(tok) => tokio::select! {
                biased;
                _ = tok.cancelled() => return Ok(()),
                r = send_fut => r?,
            },
            None => send_fut.await?,
        };
        let resp = resp.error_for_status()?;

        let mut splitter = ThinkSplitter::new();
        let mut events = resp.bytes_stream().eventsource();

        loop {
            let next = match &cancel {
                Some(tok) => tokio::select! {
                    biased;
                    _ = tok.cancelled() => return Ok(()),
                    item = events.next() => item,
                },
                None => events.next().await,
            };
            let Some(item) = next else { break };
            let ev = match item {
                Ok(e) => e,
                Err(e) => return Err(anyhow!("sse decode: {e}")),
            };
            if ev.data == "[DONE]" {
                break;
            }
            if let Some(chunk) = parse_sse_event(&ev.data, &mut splitter) {
                if tx.send(chunk).is_err() {
                    break;
                }
            }
        }
        Ok(())
    }
}
