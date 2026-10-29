//! Slack Web API calls and the Socket Mode connection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(90);

/// `PRIME_BOT_SLACK_API_URL` overrides the Web API base URL (default `https://slack.com/api`).
pub fn api_base() -> String {
    std::env::var("PRIME_BOT_SLACK_API_URL").unwrap_or_else(|_| "https://slack.com/api".to_string())
}

pub struct Api {
    http: reqwest::Client,
    download_http: reqwest::Client,
    base: String,
    token: String,
    last_post: Mutex<HashMap<String, Instant>>,
}

impl Api {
    pub fn new(http: reqwest::Client, base: String, token: String) -> Api {
        Api {
            http,
            download_http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("building private download client"),
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

    /// Downloads one hosted image. Errors deliberately omit URLs and response bodies.
    pub async fn download_image(
        &self,
        url: &str,
        expected_mime: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let url = private_file_url(url, &self.base)?;
        let mut response = self
            .download_http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| anyhow!("image download request failed"))?;
        if response.status().is_redirection() {
            bail!("image download redirect refused");
        }
        if !response.status().is_success() {
            bail!(
                "image download returned HTTP {}",
                response.status().as_u16()
            );
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(str::trim);
        if mime != Some(expected_mime) {
            bail!("image response MIME type does not match metadata");
        }
        if response
            .content_length()
            .is_some_and(|n| n > max_bytes as u64)
        {
            bail!("image exceeds size limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("image download body failed"))?
        {
            if chunk.len() > max_bytes.saturating_sub(bytes.len()) {
                bail!("image exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// Uploads a local file with Slack's external upload flow and shares it to a conversation.
    pub async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &std::path::Path,
        title: Option<&str>,
        text: Option<&str>,
    ) -> Result<Value> {
        tokio::time::timeout(
            UPLOAD_TIMEOUT,
            self.upload_file_inner(channel, thread_ts, path, title, text),
        )
        .await
        .map_err(|_| anyhow!("file upload exceeded 90 seconds; completion may be uncertain"))?
    }

    async fn upload_file_inner(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        path: &std::path::Path,
        title: Option<&str>,
        text: Option<&str>,
    ) -> Result<Value> {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow!("upload path needs a UTF-8 filename"))?;
        let bytes = read_upload_file(path).await?;
        let length = bytes.len().to_string();
        let ticket = self
            .call(
                "files.getUploadURLExternal",
                &[("filename", filename), ("length", &length)],
            )
            .await?;
        let file_id = ticket["file_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("files.getUploadURLExternal returned no file_id"))?;
        let upload_url = ticket["upload_url"]
            .as_str()
            .ok_or_else(|| anyhow!("files.getUploadURLExternal returned no upload_url"))?;
        let upload_url = private_file_url(upload_url, &self.base)
            .map_err(|_| anyhow!("files.getUploadURLExternal returned an unsafe upload URL"))?;
        // The signed upload URL needs no bearer token and must never redirect.
        let response = self
            .download_http
            .post(upload_url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|_| anyhow!("file upload request failed"))?;
        if response.status() != reqwest::StatusCode::OK {
            bail!("file upload returned HTTP {}", response.status().as_u16());
        }
        let title = title.unwrap_or(filename);
        let files = json!([{ "id": file_id, "title": title }]).to_string();
        let mut params = vec![("files", files.as_str()), ("channel_id", channel)];
        if let Some(ts) = thread_ts {
            params.push(("thread_ts", ts));
        }
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            params.push(("initial_comment", text));
        }
        let body = self.call("files.completeUploadExternal", &params).await?;
        let completed = body["files"]
            .as_array()
            .ok_or_else(|| anyhow!("files.completeUploadExternal returned no files"))?;
        let file = completed
            .iter()
            .find(|file| file["id"].as_str() == Some(file_id))
            .ok_or_else(|| anyhow!("files.completeUploadExternal returned no uploaded file"))?;
        Ok(json!([{
            "id": file_id,
            "title": file["title"].as_str().unwrap_or(title),
            "permalink": file["permalink"].as_str(),
        }]))
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

async fn read_upload_file(path: &std::path::Path) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        bail!("upload path must be absolute");
    }
    let metadata = tokio::fs::metadata(path)
        .await
        .context("reading upload file metadata")?;
    // Check before open so directories and FIFOs are rejected without reading them.
    if !metadata.is_file() {
        bail!("upload path must be a regular file");
    }
    if metadata.len() == 0 {
        bail!("upload file is empty");
    }
    if metadata.len() > MAX_UPLOAD_BYTES {
        bail!("upload file exceeds 20 MiB limit");
    }
    let file = tokio::fs::File::open(path)
        .await
        .context("opening upload file")?;
    let opened_metadata = file
        .metadata()
        .await
        .context("reading upload file metadata")?;
    if !opened_metadata.is_file() {
        bail!("upload path must be a regular file");
    }
    if opened_metadata.len() == 0 {
        bail!("upload file is empty");
    }
    if opened_metadata.len() > MAX_UPLOAD_BYTES {
        bail!("upload file exceeds 20 MiB limit");
    }
    let mut bytes = Vec::new();
    let mut bounded = tokio::io::AsyncReadExt::take(file, MAX_UPLOAD_BYTES + 1);
    tokio::io::AsyncReadExt::read_to_end(&mut bounded, &mut bytes)
        .await
        .context("reading upload file")?;
    if bytes.is_empty() {
        bail!("upload file is empty");
    }
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        bail!("upload file exceeds 20 MiB limit");
    }
    Ok(bytes)
}

/// Only Slack's file origin, or the exact explicitly configured loopback fixture origin.
fn private_file_url(raw: &str, api_base: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow!("invalid image URL"))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("unsafe image URL");
    }
    let production = url.scheme() == "https"
        && url.host_str() == Some("files.slack.com")
        && url.port_or_known_default() == Some(443);
    let fixture = reqwest::Url::parse(api_base).ok().is_some_and(|base| {
        let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        loopback
            && matches!(base.scheme(), "http" | "https")
            && base.username().is_empty()
            && base.password().is_none()
            && url.origin() == base.origin()
    });
    if !production && !fixture {
        bail!("image URL origin is not allowed");
    }
    Ok(url)
}

#[cfg(test)]
mod image_tests {
    use super::private_file_url;

    #[test]
    fn private_urls_require_exact_slack_origin() {
        let base = "https://slack.com/api";
        assert!(private_file_url("https://files.slack.com/files-pri/x.png", base).is_ok());
        for url in [
            "http://files.slack.com/x",
            "https://files.slack.com.evil.test/x",
            "https://sub.files.slack.com/x",
            "https://files.slack.com:444/x",
            "https://token@files.slack.com/x",
            "https://files.slack.com/x#fragment",
            "https://127.0.0.1/x",
        ] {
            assert!(private_file_url(url, base).is_err(), "{url}");
        }
    }

    #[test]
    fn fixture_requires_same_loopback_origin() {
        let base = "http://127.0.0.1:12345/api";
        assert!(private_file_url("http://127.0.0.1:12345/files/x", base).is_ok());
        assert!(private_file_url("http://127.0.0.1:12346/files/x", base).is_err());
        assert!(private_file_url("http://localhost:12345/files/x", base).is_err());
        assert!(
            private_file_url(
                "http://evil.test:12345/files/x",
                "http://evil.test:12345/api"
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod upload_tests {
    use super::{MAX_UPLOAD_BYTES, private_file_url, read_upload_file};
    use tempfile::tempdir;

    #[tokio::test]
    async fn upload_reads_binary_and_rejects_invalid_local_paths() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let payload = [0, 255, 1, 128];
        std::fs::write(&path, payload).unwrap();
        assert_eq!(read_upload_file(&path).await.unwrap(), payload);
        assert!(
            read_upload_file(std::path::Path::new("relative.bin"))
                .await
                .is_err()
        );
        assert!(read_upload_file(dir.path()).await.is_err());
        assert!(read_upload_file(&dir.path().join("missing")).await.is_err());
    }

    #[tokio::test]
    async fn upload_rejects_empty_and_oversize_files_but_accepts_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let file = std::fs::File::create(&path).unwrap();
        assert!(read_upload_file(&path).await.is_err());
        file.set_len(MAX_UPLOAD_BYTES + 1).unwrap();
        assert!(read_upload_file(&path).await.is_err());
        file.set_len(MAX_UPLOAD_BYTES).unwrap();
        assert_eq!(
            read_upload_file(&path).await.unwrap().len() as u64,
            MAX_UPLOAD_BYTES
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_rejects_nonregular_files_before_open() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(read_upload_file(&path).await.is_err());
    }

    #[test]
    fn signed_upload_urls_allow_query_but_not_credentials_or_other_origins() {
        let base = "https://slack.com/api";
        assert!(
            private_file_url(
                "https://files.slack.com/upload/v1/abc?signature=secret",
                base
            )
            .is_ok()
        );
        for url in [
            "https://user:secret@files.slack.com/upload/v1/abc",
            "https://files.slack.com/upload/v1/abc#fragment",
            "https://slack.com/upload/v1/abc",
            "https://files.slack.com.evil.test/upload/v1/abc",
        ] {
            assert!(private_file_url(url, base).is_err());
        }
    }
}
