use std::path::PathBuf;

use anyhow::{Context, Result};

/// The prime-bot home directory (`$PRIME_BOT_HOME`, default `~/.prime-bot`).
#[derive(Clone, Debug)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn from_env() -> Result<Home> {
        let root = match std::env::var_os("PRIME_BOT_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".prime-bot"),
        };
        Ok(Home { root })
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.yaml")
    }

    pub fn secrets(&self) -> PathBuf {
        self.root.join("secrets.yaml")
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("state.sqlite")
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("prime-bot.sock")
    }

    pub fn bot_dir(&self, name: &str) -> PathBuf {
        self.root.join("bots").join(name)
    }
}
