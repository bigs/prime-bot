use std::fmt;

use anyhow::{Result, bail};

/// A thread (`<channel>/<thread_ts>`) or a DM outside any thread (`<channel>`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Conversation {
    pub channel: String,
    pub thread_ts: Option<String>,
}

impl Conversation {
    pub fn thread(channel: &str, thread_ts: &str) -> Conversation {
        Conversation {
            channel: channel.to_string(),
            thread_ts: Some(thread_ts.to_string()),
        }
    }

    pub fn dm(channel: &str) -> Conversation {
        Conversation {
            channel: channel.to_string(),
            thread_ts: None,
        }
    }

    pub fn parse(key: &str) -> Result<Conversation> {
        let (channel, thread_ts) = match key.split_once('/') {
            Some((c, t)) => (c, Some(t)),
            None => (key, None),
        };
        if channel.is_empty() || thread_ts == Some("") {
            bail!("invalid conversation {key:?}: expected <channel>/<thread_ts> or <dm channel>");
        }
        if thread_ts.is_none() && !crate::config::is_dm_channel(channel) {
            bail!("invalid conversation {key:?}: only DM channels (D...) may omit the thread_ts");
        }
        Ok(Conversation {
            channel: channel.to_string(),
            thread_ts: thread_ts.map(str::to_string),
        })
    }

    pub fn kind(&self) -> &'static str {
        if self.thread_ts.is_some() {
            "thread"
        } else {
            "dm"
        }
    }
}

impl fmt::Display for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.thread_ts {
            Some(ts) => write!(f, "{}/{}", self.channel, ts),
            None => write!(f, "{}", self.channel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for key in ["C1/1730000000.1234", "D9"] {
            assert_eq!(Conversation::parse(key).unwrap().to_string(), key);
        }
        assert!(Conversation::parse("C1").is_err());
        assert!(Conversation::parse("C1/").is_err());
        assert!(Conversation::parse("").is_err());
    }
}
