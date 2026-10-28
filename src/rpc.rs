//! JSON-RPC 2.0 over a Unix socket, one JSON object per line.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::Daemon;

pub async fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("a prime-bot daemon is already running ({})", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(stream, daemon).await {
                        tracing::debug!("rpc connection: {e:#}");
                    }
                });
            }
            Err(e) => tracing::error!("rpc accept: {e}"),
        }
    }
}

async fn connection(stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = respond(&daemon, &line).await;
        write.write_all(format!("{response}\n").as_bytes()).await?;
    }
    Ok(())
}

async fn respond(daemon: &Arc<Daemon>, line: &str) -> Value {
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return error(Value::Null, -32700, &format!("parse error: {e}")),
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = request["method"].as_str() else {
        return error(id, -32600, "invalid request: missing method");
    };
    tracing::debug!(%method, "rpc");
    match daemon
        .handle(
            method,
            request.get("params").cloned().unwrap_or(Value::Null),
        )
        .await
    {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => error(id, e.code, &e.message),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Client side: one request on a fresh connection.
pub async fn call(socket: &Path, method: &str, params: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket).await.map_err(|_| {
        anyhow!("prime-bot daemon is not running; start it with `prime-bot daemon`")
    })?;
    let (read, mut write) = stream.into_split();
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    write.write_all(format!("{request}\n").as_bytes()).await?;
    let line = BufReader::new(read)
        .lines()
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("daemon closed the connection"))?;
    let response: Value = serde_json::from_str(&line)?;
    if let Some(err) = response.get("error") {
        bail!("{}", err["message"].as_str().unwrap_or("unknown error"));
    }
    Ok(response["result"].clone())
}
