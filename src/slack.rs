//! Slack Web API calls and the Socket Mode connection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// `PRIME_BOT_SLACK_API_URL` overrides the Web API base URL (default `https://slack.com/api`).
pub fn api_base() -> String {
    std::env::var("PRIME_BOT_SLACK_API_URL").unwrap_or_else(|_| "https://slack.com/api".to_string())
}

pub struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
    last_post: Mutex<HashMap<String, Instant>>,
}

impl Api {
    pub fn new(http: reqwest::Client, base: String, token: String) -> Api {
        Api {
            http,
            base,
            token,
            last_post: Mutex::new(HashMap::new()),
        }
    }

    /// Calls a Web API method with form parameters. Retries on rate limits.
    pub async fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        for _ in 0..3 {
            let resp = self
                .http
                .post(format!("{}/{}", self.base, method))
                .bearer_auth(&self.token)
                .form(params)
                .send()
                .await
                .with_context(|| format!("Slack {method}"))?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let body: Value = resp
                .json()
                .await
                .with_context(|| format!("Slack {method}: bad response"))?;
            if body["ok"].as_bool() != Some(true) {
                bail!(
                    "Slack {method} failed: {}",
                    body["error"].as_str().unwrap_or("unknown error")
                );
            }
            return Ok(body);
        }
        bail!("Slack {method}: rate limited")
    }

    /// Posts a message, keeping at least one second between posts to the same channel.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<String> {
        {
            let mut last = self.last_post.lock().await;
            if let Some(t) = last.get(channel) {
                let since = t.elapsed();
                if since < Duration::from_secs(1) {
                    tokio::time::sleep(Duration::from_secs(1) - since).await;
                }
            }
            last.insert(channel.to_string(), Instant::now());
        }
        let mut params = vec![("channel", channel), ("text", text)];
        if let Some(ts) = thread_ts {
            params.push(("thread_ts", ts));
        }
        let body = self.call("chat.postMessage", &params).await?;
        body["ts"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("chat.postMessage returned no ts"))
    }
}

/// Opens one Socket Mode session and forwards `events_api` payloads until it ends.
/// Returns Ok when Slack asks for a reconnect.
pub async fn run_socket(
    http: &reqwest::Client,
    base: &str,
    app_token: &str,
    events: &mpsc::Sender<Value>,
    on_hello: impl Fn(),
) -> Result<()> {
    let open = Api::new(http.clone(), base.to_string(), app_token.to_string());
    let body = open.call("apps.connections.open", &[]).await?;
    let url = body["url"]
        .as_str()
        .ok_or_else(|| anyhow!("apps.connections.open returned no url"))?;
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .context("Socket Mode connect")?;
    let (mut sink, mut stream) = ws.split();
    while let Some(frame) = stream.next().await {
        let text = match frame.context("Socket Mode read")? {
            WsMessage::Text(t) => t,
            WsMessage::Ping(p) => {
                sink.send(WsMessage::Pong(p)).await?;
                continue;
            }
            WsMessage::Close(_) => return Ok(()),
            _ => continue,
        };
        let envelope: Value = match serde_json::from_str(text.as_str()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("ignoring bad Socket Mode frame: {e}");
                continue;
            }
        };
        if let Some(id) = envelope["envelope_id"].as_str() {
            sink.send(WsMessage::text(json!({ "envelope_id": id }).to_string()))
                .await?;
        }
        match envelope["type"].as_str() {
            Some("hello") => on_hello(),
            Some("disconnect") => return Ok(()),
            Some("events_api") if events.send(envelope["payload"].clone()).await.is_err() => {
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}
