//! Backend IPC: accept exactly one client on the supplied pipe name,
//! return split read/write halves wrapped in length-delimited codecs.

use anyhow::{Context, Result};
use bytes::Bytes;
use interprocess::local_socket::{
    tokio::{prelude::*, Stream as IpcStream},
    GenericNamespaced, ListenerOptions, ToNsName,
};
use tokio::io::{ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::info;

pub type IpcReader = ReadHalf<IpcStream>;
pub type IpcWriter = WriteHalf<IpcStream>;

pub async fn accept(pipe_name: &str) -> Result<(IpcReader, IpcWriter)> {
    // On Windows, callers typically pass the raw `\\.\pipe\xxx` form.
    // `interprocess` 2.x prefers the namespaced form: strip the prefix and
    // let `GenericNamespaced` re-add it.
    let raw = pipe_name
        .strip_prefix(r"\\.\pipe\")
        .unwrap_or(pipe_name);
    let name = raw
        .to_ns_name::<GenericNamespaced>()
        .context("invalid pipe name")?;

    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .context("create listener")?;

    info!("waiting for FE to connect");
    let stream = listener.accept().await.context("accept")?;
    info!("FE connected");

    let (r, w) = tokio::io::split(stream);
    Ok((r, w))
}

pub fn framed_read(r: IpcReader) -> FramedRead<IpcReader, LengthDelimitedCodec> {
    FramedRead::new(r, LengthDelimitedCodec::new())
}

pub fn framed_write(w: IpcWriter) -> FramedWrite<IpcWriter, LengthDelimitedCodec> {
    FramedWrite::new(w, LengthDelimitedCodec::new())
}

/// Convenience: the writer accepts `Bytes`, not Vec<u8>.
#[allow(dead_code)]
pub fn to_bytes(v: Vec<u8>) -> Bytes {
    Bytes::from(v)
}
