//! Token-count helpers. Calls `/tokenize` for an exact count; falls back to a
//! conservative `chars / 4` heuristic when the endpoint isn't reachable.

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct TokenizeReq<'a> {
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct TokenizeResp {
    tokens: Vec<i64>,
}

pub async fn tokenize_exact(base_url: &str, text: &str) -> Result<u32> {
    let url = format!("{}/tokenize", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(url)
        .json(&TokenizeReq { content: text })
        .send()
        .await?
        .error_for_status()?
        .json::<TokenizeResp>()
        .await?;
    Ok(resp.tokens.len() as u32)
}

/// Heuristic: ~4 chars per token. Reliable enough for live UI updates between
/// the bracketing exact counts at turn start and turn end.
pub fn approx_tokens(text: &str) -> u32 {
    let n = text.chars().count();
    ((n + 3) / 4) as u32
}
