use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub bots: BTreeMap<String, BotConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BotConfig {
    pub owners: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default = "default_dms")]
    pub dms: bool,
}

fn default_dms() -> bool {
    true
}

impl BotConfig {
    pub fn is_owner(&self, user: &str) -> bool {
        self.owners.iter().any(|u| u == user)
    }

    /// Owners and allowed users. Other prime-bot bots are checked separately.
    pub fn is_allowed_person(&self, user: &str) -> bool {
        self.is_owner(user) || self.allowed_users.iter().any(|u| u == user)
    }
}

pub fn is_dm_channel(channel: &str) -> bool {
    channel.starts_with('D')
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct BotSecrets {
    pub app_token: Option<String>,
    pub bot_token: Option<String>,
}

pub type Secrets = BTreeMap<String, BotSecrets>;

pub fn validate_name(name: &str) -> Result<()> {
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if name.is_empty()
        || name.len() > 32
        || !ok_chars
        || name.starts_with('-')
        || name.ends_with('-')
    {
        bail!(
            "invalid bot name {name:?}: use 1-32 characters a-z, 0-9 and '-', not starting or ending with '-'"
        );
    }
    Ok(())
}

pub fn load<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(T::default()),
        Ok(text) => {
            serde_yaml_ng::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes YAML atomically. `private` files get mode 0600.
pub fn save<T: Serialize>(path: &Path, value: &T, private: bool) -> Result<()> {
    let text = serde_yaml_ng::to_string(value)?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if private {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("atlas").is_ok());
        assert!(validate_name("bot-2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-x").is_err());
        assert!(validate_name("Atlas").is_err());
        assert!(validate_name("a_b").is_err());
    }

    #[test]
    fn config_roundtrip_and_defaults() {
        let cfg: Config = serde_yaml_ng::from_str("bots:\n  atlas:\n    owners: [U1]\n").unwrap();
        let bot = &cfg.bots["atlas"];
        assert!(bot.dms);
        assert!(bot.allowed_users.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        save(&path, &cfg, false).unwrap();
        assert_eq!(load::<Config>(&path).unwrap(), cfg);
    }

    #[test]
    fn secrets_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yaml");
        let mut s = Secrets::new();
        s.insert("atlas".into(), BotSecrets::default());
        save(&path, &s, true).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(load::<Secrets>(&path).unwrap(), s);
    }
}
