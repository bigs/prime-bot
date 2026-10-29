//! Bounded Slack image enrichment and private local attachment storage.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use tokio::time::{Instant, timeout_at};

use crate::config;
use crate::paths::Home;
use crate::slack::Api;

pub const MAX_IMAGES: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const ENRICHMENT_TIMEOUT: Duration = Duration::from_secs(15);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub struct ImageBatch {
    pub images: Vec<PathBuf>,
    pub notes: Vec<String>,
}

/// Called only after the message has passed incoming routing rules.
pub async fn save_images(home: &Home, bot: &str, api: &Api, files: &[Value]) -> ImageBatch {
    let mut batch = ImageBatch::default();
    if files.is_empty() {
        return batch;
    }
    let directory = match attachment_directory(home, bot) {
        Ok(path) => path,
        Err(_) => {
            batch.notes.push("image storage is unavailable".into());
            return batch;
        }
    };
    let deadline = Instant::now() + ENRICHMENT_TIMEOUT;
    let mut seen = HashSet::new();
    let mut attempted = 0;
    let mut metadata_lookups = 0;
    for original in files {
        if is_external(original) {
            batch.notes.push("remote files are not supported".into());
            continue;
        }
        let Some(id) = original["id"].as_str().filter(|id| valid_file_id(id)) else {
            batch.notes.push("file has no safe Slack file ID".into());
            continue;
        };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let mut file = original.clone();
        let mime = file["mimetype"].as_str();
        let needs_info = file["file_access"] == "check_file_info"
            || mime.is_none()
            || (mime.is_some_and(|m| image_extension(m).is_some()) && private_url(&file).is_none());
        if needs_info {
            if metadata_lookups >= MAX_IMAGES {
                batch
                    .notes
                    .push("additional file metadata skipped (4-file lookup limit)".into());
                continue;
            }
            metadata_lookups += 1;
            match timeout_at(deadline, api.call("files.info", &[("file", id)])).await {
                Ok(Ok(body)) if body["file"]["id"].as_str() == Some(id) => {
                    file = body["file"].clone()
                }
                Ok(_) => {
                    batch
                        .notes
                        .push(format!("{id}: image metadata is unavailable"));
                    continue;
                }
                Err(_) => {
                    batch
                        .notes
                        .push("image preparation timed out (15-second limit)".into());
                    break;
                }
            }
        }
        if is_external(&file) {
            batch
                .notes
                .push(format!("{id}: remote files are not supported"));
            continue;
        }
        let Some(mime) = file["mimetype"].as_str() else {
            batch
                .notes
                .push(format!("{id}: image metadata is unavailable"));
            continue;
        };
        let Some(extension) = image_extension(mime) else {
            let reason = if mime.starts_with("image/") {
                "unsupported image type"
            } else {
                "nonimage files are not supported"
            };
            batch.notes.push(format!("{id}: {reason}"));
            continue;
        };
        if attempted >= MAX_IMAGES {
            batch
                .notes
                .push("additional images skipped (4-image limit)".into());
            break;
        }
        attempted += 1;
        if file["size"]
            .as_u64()
            .is_some_and(|n| n > MAX_IMAGE_BYTES as u64)
        {
            batch
                .notes
                .push(format!("{id}: image exceeds 10 MiB limit"));
            continue;
        }
        let path = directory.join(format!("{id}.{extension}"));
        match timeout_at(deadline, save_image(api, &file, mime, &path)).await {
            Ok(Ok(())) => batch.images.push(path),
            Ok(Err(error)) => batch.notes.push(format!("{id}: {error}")),
            Err(_) => {
                batch
                    .notes
                    .push("image preparation timed out (15-second limit)".into());
                break;
            }
        }
    }
    batch
}

fn valid_file_id(id: &str) -> bool {
    id.len() >= 2
        && id.len() <= 64
        && id.starts_with('F')
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_external(file: &Value) -> bool {
    file["is_external"] == true || file["mode"] == "external" || file["filetype"] == "remote"
}

fn private_url(file: &Value) -> Option<&str> {
    file["url_private_download"]
        .as_str()
        .or_else(|| file["url_private"].as_str())
}

fn image_extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn image_signature(bytes: &[u8], mime: &str) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        _ => false,
    }
}

fn private_directory(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => bail!("image storage directory could not be created"),
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| anyhow!("image storage is unavailable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("image storage directory is unsafe");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| anyhow!("image storage permissions could not be set"))?;
    Ok(())
}

fn attachment_directory(home: &Home, bot: &str) -> Result<PathBuf> {
    config::validate_name(bot).map_err(|_| anyhow!("invalid bot name"))?;
    let root = if home.root.is_absolute() {
        home.root.clone()
    } else {
        std::env::current_dir()
            .map_err(|_| anyhow!("image storage is unavailable"))?
            .join(&home.root)
    };
    let attachments = root.join("attachments");
    private_directory(&attachments)?;
    let directory = attachments.join(bot);
    private_directory(&directory)?;
    Ok(directory)
}

async fn save_image(api: &Api, file: &Value, mime: &str, path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("cached image path is unsafe");
            }
            if metadata.len() <= MAX_IMAGE_BYTES as u64 {
                let bytes =
                    fs::read(path).map_err(|_| anyhow!("cached image could not be read"))?;
                if image_signature(&bytes, mime) {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                        .map_err(|_| anyhow!("cached image permissions could not be set"))?;
                    return Ok(());
                }
            }
            fs::remove_file(path)
                .map_err(|_| anyhow!("invalid cached image could not be removed"))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => bail!("cached image could not be checked"),
    }
    let url = private_url(file).ok_or_else(|| anyhow!("private image URL is unavailable"))?;
    let bytes = api.download_image(url, mime, MAX_IMAGE_BYTES).await?;
    if !image_signature(&bytes, mime) {
        bail!("download is not the declared image type");
    }
    write_image(path, &bytes)
}

fn write_image(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!(
        "part-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| anyhow!("image cache file could not be created"))?;
    let result = (|| {
        output
            .write_all(bytes)
            .map_err(|_| anyhow!("image cache write failed"))?;
        output
            .sync_all()
            .map_err(|_| anyhow!("image cache write failed"))?;
        fs::hard_link(&temp, path).map_err(|_| anyhow!("image cache publish failed"))?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;

    #[test]
    fn output_components_and_types_are_restricted() {
        assert!(valid_file_id("F0123ABC"));
        for id in ["../F123", "F/123", "F123.png", "F123\n", "", "C123"] {
            assert!(!valid_file_id(id));
        }
        assert_eq!(image_extension("image/jpeg"), Some("jpg"));
        assert_eq!(image_extension("image/svg+xml"), None);
        assert!(is_external(&json!({"mode":"external"})));
        assert!(is_external(&json!({"is_external":true})));
        assert!(is_external(&json!({"filetype":"remote"})));
        assert!(!image_signature(b"<html>error</html>", "image/png"));
    }

    #[test]
    fn storage_is_private_and_rejects_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home {
            root: temp.path().to_owned(),
        };
        let dir = attachment_directory(&home, "atlas").unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = dir.join("F123.png");
        write_image(&path, b"example").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_image(&path, b"replacement").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"example");
        assert!(attachment_directory(&home, "../escape").is_err());
        symlink(temp.path(), home.root.join("attachments").join("linked")).unwrap();
        assert!(attachment_directory(&home, "linked").is_err());
    }
}
