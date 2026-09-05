//! A scripted fault server for the LLM seam (guide §14.2).
//!
//! `retry` is unit-tested against fabricated errors, which proves the
//! classifier and nothing else. The half that only a socket can prove is
//! whether [`LlmClient::chat_stream`] turns a real 429, a real reset
//! mid-body, or a real malformed SSE frame into the error the classifier
//! expects — and whether a retry loop wrapped around it actually converges.
//!
//! So: a hand-rolled HTTP/1.1 server on a loopback port, serving
//! `/v1/chat/completions`, `/v1/models` and `/props` from a queue of
//! [`Behaviour`]s. One behaviour is consumed per connection, so a script is
//! literally "what the provider does on the 1st, 2nd, 3rd call".
//!
//! Hand-rolled rather than axum-behind-a-dev-dependency because the faults
//! that matter here are *below* what a framework will let you express: a
//! connection closed halfway through a chunk, a body that never arrives,
//! a `data:` frame that is not JSON. Serving those means owning the socket.
//!
//! `#[cfg(any(test, feature = "mock"))]`: it is test scaffolding, and the
//! feature exists so `backend`'s own tests can drive it too.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// What the server does for one connection.
#[derive(Debug, Clone)]
pub enum Behaviour {
    /// A complete streamed answer: one `data:` frame per delta, then a
    /// usage frame, then `[DONE]`.
    Success { deltas: Vec<String> },
    /// A streamed answer that calls a tool, in the OpenAI native shape.
    ToolCall { id: String, name: String, arguments: String },
    /// Headers and some frames, then the connection drops mid-body — the
    /// shape a proxy timing out produces, and the one a client is most
    /// likely to mistake for a clean end of stream.
    ResetMidBody { deltas: Vec<String> },
    /// A `data:` frame whose payload is not JSON.
    MalformedChunk,
    /// `429` with a `Retry-After` header.
    RateLimited { retry_after_secs: u64 },
    /// A plain server error.
    ServerError { status: u16, body: String },
    /// Accept the connection and never answer, for `hold`. The client's own
    /// read timeout is what ends it.
    Stall { hold: Duration },
}

impl Behaviour {
    /// The common case, as one delta.
    pub fn ok(text: &str) -> Self {
        Behaviour::Success { deltas: vec![text.to_string()] }
    }
}

/// A running mock provider. Dropping it stops the accept loop.
pub struct MockServer {
    /// `http://127.0.0.1:<port>` — pass straight to `LlmClient::new`.
    pub base_url: String,
    /// Behaviours left unconsumed. A test asserts on this to prove a retry
    /// actually made the second call rather than answering from thin air.
    remaining: Arc<Mutex<Vec<Behaviour>>>,
    /// Requests served, in order, as `(path, body)`.
    seen: Arc<Mutex<Vec<(String, String)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockServer {
    /// Bind an ephemeral port and serve `script`, one behaviour per
    /// connection. A connection past the end of the script gets a
    /// `Success` with no deltas — running out of script is a test bug, and
    /// hanging would hide it behind a timeout.
    pub async fn start(script: Vec<Behaviour>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let remaining = Arc::new(Mutex::new(script));
        let seen = Arc::new(Mutex::new(Vec::new()));

        let q = remaining.clone();
        let log = seen.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let q = q.clone();
                let log = log.clone();
                // One task per connection: a `Stall` must not block the
                // next request, or a test for "the retry happened" would
                // deadlock instead of failing.
                tokio::spawn(async move {
                    let _ = serve(stream, q, log).await;
                });
            }
        });

        Ok(Self {
            base_url: format!("http://127.0.0.1:{port}"),
            remaining,
            seen,
            task,
        })
    }

    /// Behaviours the script has not served yet.
    pub async fn remaining(&self) -> usize {
        self.remaining.lock().await.len()
    }

    /// The requests served, in order.
    pub async fn requests(&self) -> Vec<(String, String)> {
        self.seen.lock().await.clone()
    }

    pub async fn request_count(&self) -> usize {
        self.seen.lock().await.len()
    }
}

async fn serve(
    mut stream: TcpStream,
    queue: Arc<Mutex<Vec<Behaviour>>>,
    seen: Arc<Mutex<Vec<(String, String)>>>,
) -> std::io::Result<()> {
    let (path, body) = match read_request(&mut stream).await? {
        Some(r) => r,
        // A client that connects and says nothing (a probe) is not a
        // request, and must not eat a behaviour.
        None => return Ok(()),
    };
    seen.lock().await.push((path.clone(), body));

    // The discovery endpoints are not part of the fault script: a test
    // about retries should not have to script the context-window probe.
    if path.starts_with("/v1/models") {
        return write_json(
            &mut stream,
            200,
            r#"{"data":[{"id":"mock","max_model_len":8192}]}"#,
        )
        .await;
    }
    if path.starts_with("/props") || path.starts_with("/health") {
        return write_json(&mut stream, 404, r#"{"error":"not this provider"}"#).await;
    }

    let behaviour = queue
        .lock()
        .await
        .pop()
        .unwrap_or(Behaviour::Success { deltas: Vec::new() });

    match behaviour {
        Behaviour::Success { deltas } => {
            write_sse_head(&mut stream).await?;
            for d in &deltas {
                write_frame(&mut stream, &content_frame(d)).await?;
            }
            write_frame(&mut stream, &usage_frame(11, 7)).await?;
            write_frame(&mut stream, "[DONE]").await?;
        }
        Behaviour::ToolCall { id, name, arguments } => {
            write_sse_head(&mut stream).await?;
            write_frame(&mut stream, &tool_frame(&id, &name, &arguments)).await?;
            write_frame(&mut stream, &usage_frame(11, 7)).await?;
            write_frame(&mut stream, "[DONE]").await?;
        }
        Behaviour::ResetMidBody { deltas } => {
            write_sse_head(&mut stream).await?;
            for d in &deltas {
                write_frame(&mut stream, &content_frame(d)).await?;
            }
            // No `[DONE]`, no close frame: just go away. Chunked encoding
            // makes this a *truncated* body rather than a complete one,
            // which is the whole point of the case.
            stream.write_all(b"1e\r\ndata: {\"choices\":[{\"delta").await?;
            stream.flush().await?;
            drop(stream);
            return Ok(());
        }
        Behaviour::MalformedChunk => {
            write_sse_head(&mut stream).await?;
            write_frame(&mut stream, "{not json at all}").await?;
            write_frame(&mut stream, "[DONE]").await?;
        }
        Behaviour::RateLimited { retry_after_secs } => {
            let body = r#"{"error":{"message":"slow down","type":"rate_limit"}}"#;
            let head = format!(
                "HTTP/1.1 429 Too Many Requests\r\n\
                 content-type: application/json\r\n\
                 retry-after: {retry_after_secs}\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body.as_bytes()).await?;
        }
        Behaviour::ServerError { status, body } => {
            return write_json(&mut stream, status, &body).await;
        }
        Behaviour::Stall { hold } => {
            tokio::time::sleep(hold).await;
            return Ok(());
        }
    }
    stream.flush().await?;
    Ok(())
}

/// Read one HTTP request. Returns the path and the body, or `None` when the
/// peer closed without sending a request line.
async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, String)>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // Header block. Byte at a time is fine at this scale and avoids
    // over-reading into a body we then have to hand back.
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await? {
            0 if buf.is_empty() => return Ok(None),
            0 => break,
            _ => buf.push(byte[0]),
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut body).await?;
    }
    Ok(Some((path, String::from_utf8_lossy(&body).to_string())))
}

async fn write_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Chunked so the body can be cut off mid-frame — a `content-length`
/// response cannot express `ResetMidBody`.
async fn write_sse_head(stream: &mut TcpStream) -> std::io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              content-type: text/event-stream\r\n\
              cache-control: no-cache\r\n\
              transfer-encoding: chunked\r\n\r\n",
        )
        .await
}

async fn write_frame(stream: &mut TcpStream, data: &str) -> std::io::Result<()> {
    let frame = format!("data: {data}\n\n");
    stream.write_all(format!("{:x}\r\n", frame.len()).as_bytes()).await?;
    stream.write_all(frame.as_bytes()).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

fn content_frame(delta: &str) -> String {
    serde_json::json!({
        "choices": [{ "index": 0, "delta": { "content": delta } }]
    })
    .to_string()
}

fn tool_frame(id: &str, name: &str, arguments: &str) -> String {
    serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": { "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            }]},
            "finish_reason": "tool_calls"
        }]
    })
    .to_string()
}

fn usage_frame(prompt: u32, completion: u32) -> String {
    serde_json::json!({
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::LlmClient;
    use crate::retry;

    async fn collect(
        client: &LlmClient,
    ) -> (anyhow::Result<()>, Vec<crate::client::StreamChunk>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let msgs = vec![crate::client::ChatMessage::text("user", "hi")];
        let res = client.chat_stream(msgs, None, tx, None).await;
        let mut chunks = Vec::new();
        while let Ok(c) = rx.try_recv() {
            chunks.push(c);
        }
        (res, chunks)
    }

    #[tokio::test]
    async fn a_scripted_success_streams_its_deltas_and_usage() {
        let server = MockServer::start(vec![Behaviour::Success {
            // Popped from the back, so a one-entry script needs no thought
            // about order; multi-entry scripts are written reversed.
            deltas: vec!["Hello".into(), ", world".into()],
        }])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);

        let (res, chunks) = collect(&client).await;
        assert!(res.is_ok(), "{res:?}");
        let text: String = chunks.iter().map(|c| c.delta_content.as_str()).collect();
        assert_eq!(text, "Hello, world");
        // The provider's own usage numbers reached the caller — the field
        // the token meter anchors on (§4.3).
        let usage = chunks.iter().find_map(|c| c.usage.as_ref()).expect("a usage chunk");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(server.remaining().await, 0);
    }

    #[tokio::test]
    async fn the_request_that_went_out_is_the_one_the_client_built() {
        let server = MockServer::start(vec![Behaviour::ok("ok")]).await.unwrap();
        let mut client = LlmClient::new(&server.base_url, "mock-model", None);
        client.max_tokens = Some(64);
        let _ = collect(&client).await;

        let reqs = server.requests().await;
        let (path, body) = reqs.last().expect("one request");
        assert_eq!(path, "/v1/chat/completions");
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["model"], "mock-model");
        assert_eq!(v["stream"], true);
        assert_eq!(v["max_tokens"], 64);
        // `include_usage` is what makes the assertion above possible.
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn a_native_tool_call_arrives_as_a_tool_delta() {
        let server = MockServer::start(vec![Behaviour::ToolCall {
            id: "call_1".into(),
            name: "read-file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        }])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);

        let (res, chunks) = collect(&client).await;
        assert!(res.is_ok(), "{res:?}");
        let call = chunks
            .iter()
            .flat_map(|c| c.delta_tool_calls.iter())
            .next()
            .expect("a tool-call delta");
        assert_eq!(call.id.as_deref(), Some("call_1"));
        assert_eq!(call.name.as_deref(), Some("read-file"));
        assert!(chunks.iter().any(|c| c.finish_reason.as_deref() == Some("tool_calls")));
    }

    #[tokio::test]
    async fn a_429_is_an_error_the_retry_classifier_calls_retryable() {
        let server = MockServer::start(vec![Behaviour::RateLimited { retry_after_secs: 1 }])
            .await
            .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);

        let (res, _) = collect(&client).await;
        let err = res.expect_err("429 must not read as success");
        // The seam the classifier actually reads: a real reqwest error from
        // a real socket, not a fabricated one.
        assert!(retry::classify(&err).is_retryable(), "429 should be retryable: {err}");
    }

    #[tokio::test]
    async fn a_500_is_retryable_and_a_400_is_not() {
        let server = MockServer::start(vec![Behaviour::ServerError {
            status: 500,
            body: r#"{"error":"boom"}"#.into(),
        }])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);
        let (res, _) = collect(&client).await;
        assert!(retry::classify(&res.unwrap_err()).is_retryable());

        let server = MockServer::start(vec![Behaviour::ServerError {
            status: 400,
            body: r#"{"error":"your messages are malformed"}"#.into(),
        }])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);
        let (res, _) = collect(&client).await;
        // A 400 is the request's fault. Retrying it is an infinite loop
        // that costs the user money and never converges.
        assert!(!retry::classify(&res.unwrap_err()).is_retryable());
    }

    #[tokio::test]
    async fn a_body_cut_mid_chunk_errors_rather_than_reading_as_a_clean_end() {
        // The failure mode worth a socket: a truncated stream that a client
        // mistakes for a finished one silently loses the tail of an answer.
        let server = MockServer::start(vec![Behaviour::ResetMidBody {
            deltas: vec!["partial".into()],
        }])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);

        let (res, chunks) = collect(&client).await;
        assert!(res.is_err(), "a truncated body must not be a success");
        // What did arrive is still delivered — the caller keeps the partial
        // answer and the error tells it the answer is partial.
        assert_eq!(
            chunks.iter().map(|c| c.delta_content.as_str()).collect::<String>(),
            "partial"
        );
    }

    #[tokio::test]
    async fn a_malformed_frame_is_skipped_and_the_stream_still_finishes() {
        // Providers emit keep-alives and vendor frames we do not model. One
        // unparseable frame must not fail a turn.
        let server = MockServer::start(vec![Behaviour::MalformedChunk]).await.unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);

        let (res, chunks) = collect(&client).await;
        assert!(res.is_ok(), "{res:?}");
        assert!(chunks.is_empty());
    }

    #[tokio::test]
    async fn a_retry_loop_over_the_socket_converges() {
        // The whole point of §14.2: retry + client + a real provider that
        // fails twice and then works. `pop` serves from the back, so the
        // success is written first.
        let server = MockServer::start(vec![
            Behaviour::ok("finally"),
            Behaviour::ServerError { status: 503, body: "{}".into() },
            Behaviour::RateLimited { retry_after_secs: 0 },
        ])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);

        let mut attempts = 0;
        let mut text = String::new();
        for attempt in 1..=3u32 {
            attempts = attempt;
            let (res, chunks) = collect(&client).await;
            match res {
                Ok(()) => {
                    text = chunks.iter().map(|c| c.delta_content.as_str()).collect();
                    break;
                }
                Err(e) => {
                    assert!(retry::classify(&e).is_retryable(), "attempt {attempt}: {e}");
                    // No sleep: the point is the loop, not the backoff,
                    // which `retry`'s own unit tests cover.
                }
            }
        }
        assert_eq!(attempts, 3);
        assert_eq!(text, "finally");
        assert_eq!(server.request_count().await, 3, "each attempt is a real call");
        assert_eq!(server.remaining().await, 0);
    }

    #[tokio::test]
    async fn the_discovery_endpoints_do_not_consume_the_script() {
        // A test about retries should not have to script the context probe.
        let server = MockServer::start(vec![Behaviour::ok("hi")]).await.unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);
        assert_eq!(client.detect_context_window().await, Some(8192));
        assert_eq!(server.remaining().await, 1, "the probe ate a behaviour");
    }

    #[tokio::test]
    async fn a_cancelled_stream_returns_without_an_error() {
        // Esc during generation is not a failure, and must not reach the
        // retry classifier as one.
        let server = MockServer::start(vec![Behaviour::Stall {
            hold: Duration::from_secs(30),
        }])
        .await
        .unwrap();
        let client = LlmClient::new(&server.base_url, "mock", None);
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let res = client
            .chat_stream(
                vec![crate::client::ChatMessage::text("user", "hi")],
                None,
                tx,
                Some(cancel),
            )
            .await;
        assert!(res.is_ok(), "a cancelled turn is not an error: {res:?}");
    }
}
