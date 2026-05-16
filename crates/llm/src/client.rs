//! HTTP client for an OpenAI-compatible /v1/chat/completions endpoint (llama.cpp).

use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

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
    pub content: String,
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

    /// Open a streaming chat completion. Each parsed `StreamChunk` is forwarded
    /// to `tx`. The task returns once the upstream stream closes or errors.
    pub async fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
        tx: mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<()> {
        let url = format!("{}/v1/chat/completions", self.base_url.trim_end_matches('/'));
        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: true,
            temperature: Some(0.7),
        };
        let resp = self
            .auth(self.http.post(url).json(&body))
            .send()
            .await?
            .error_for_status()?;

        let mut splitter = ThinkSplitter::new();
        let mut events = resp.bytes_stream().eventsource();

        while let Some(item) = events.next().await {
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
