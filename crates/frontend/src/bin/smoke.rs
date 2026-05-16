//! End-to-end smoke test: spawn backend, exchange a few frames, exit cleanly.
//! Run with: .\run.ps1 run -p frontend --bin smoke

use std::time::Duration;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use interprocess::local_socket::{
    tokio::{prelude::*, Stream as IpcStream},
    GenericNamespaced, ToNsName,
};
use protocol::{Frame, Payload, Request, Response};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

fn backend_exe() -> std::path::PathBuf {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().unwrap().parent().unwrap();
    let ext = if cfg!(windows) { ".exe" } else { "" };
    root.join("target").join("debug").join(format!("backend{ext}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let exe = backend_exe();
    anyhow::ensure!(exe.exists(), "backend not built: {}", exe.display());

    let pipe_name = format!(r"\\.\pipe\sica-rust-smoke-{}", std::process::id());
    println!("smoke: spawning BE with pipe={pipe_name}");

    let mut child = Command::new(&exe)
        .arg("--ipc")
        .arg(&pipe_name)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Connect (with brief retry — BE may take a moment to bind)
    let raw = pipe_name.strip_prefix(r"\\.\pipe\").unwrap();
    let mut stream: Option<IpcStream> = None;
    for _ in 0..40 {
        let ns = raw.to_ns_name::<GenericNamespaced>()?;
        match IpcStream::connect(ns).await {
            Ok(s) => { stream = Some(s); break; }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let stream = stream.ok_or_else(|| anyhow::anyhow!("could not connect to BE"))?;
    println!("smoke: connected");

    let (r, w) = tokio::io::split(stream);
    let mut reader = FramedRead::new(r, LengthDelimitedCodec::new());
    let mut writer = FramedWrite::new(w, LengthDelimitedCodec::new());

    // ClientHello
    let hello = Frame { id: 0, payload: Payload::ClientHello { protocol_version: protocol::PROTOCOL_VERSION } };
    writer.send(hello.encode()?.into()).await?;

    // Read server hello
    let bytes = reader.next().await.ok_or_else(|| anyhow::anyhow!("no server hello"))??;
    let frame = Frame::decode(&bytes)?;
    println!("smoke: <- {:?}", frame.payload);

    // Send IncrementCounter by 3, expect CounterValue { 3 }
    writer.send(Frame::request(1, Request::IncrementCounter { by: 3 }).encode()?.into()).await?;
    let resp = loop {
        let bytes = reader.next().await.ok_or_else(|| anyhow::anyhow!("eof"))??;
        let frame = Frame::decode(&bytes)?;
        match frame.payload {
            Payload::Response(r) if frame.id == 1 => break r,
            Payload::Event(e) => println!("smoke: event {:?}", e),
            other => println!("smoke: other {:?}", other),
        }
    };
    println!("smoke: increment(3) -> {:?}", resp);
    assert!(matches!(resp, Response::CounterValue { value: 3 }), "expected CounterValue=3, got {:?}", resp);

    // Fib(10)
    writer.send(Frame::request(2, Request::ComputeFib { n: 10 }).encode()?.into()).await?;
    let resp = loop {
        let bytes = reader.next().await.ok_or_else(|| anyhow::anyhow!("eof"))??;
        let frame = Frame::decode(&bytes)?;
        match frame.payload {
            Payload::Response(r) if frame.id == 2 => break r,
            _ => {}
        }
    };
    println!("smoke: fib(10) -> {:?}", resp);
    assert!(matches!(resp, Response::FibResult { n: 10, value: 55 }), "expected fib(10)=55, got {:?}", resp);

    // Shutdown
    writer.send(Frame::request(3, Request::Shutdown).encode()?.into()).await?;
    let _ = writer.get_mut().shutdown().await;

    // Wait for BE to exit
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;
    println!("smoke: BE exited code={:?}", status.code());

    println!("smoke: ALL OK");
    Ok(())
}
