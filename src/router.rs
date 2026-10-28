//! Decides whether one bot should receive one Slack message.

use std::collections::HashMap;

use crate::config::BotConfig;
use crate::conversation::Conversation;

/// Bot messages in a row (without a human message) after which bot messages are not delivered.
pub const LOOP_LIMIT: u32 = 4;

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub channel: String,
    pub channel_type: String,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub subtype: Option<String>,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
}

impl Message {
    pub fn from_event(event: &serde_json::Value) -> Message {
        let s = |k: &str| event.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Message {
            channel: s("channel").unwrap_or_default(),
            channel_type: s("channel_type").unwrap_or_default(),
            user: s("user"),
            bot_id: s("bot_id"),
            subtype: s("subtype"),
            text: s("text").unwrap_or_default(),
            ts: s("ts").unwrap_or_default(),
            thread_ts: s("thread_ts"),
        }
    }

    /// Normal user-visible messages, including those posted by bot users.
    pub fn is_relevant(&self) -> bool {
        matches!(
            self.subtype.as_deref(),
            None | Some("thread_broadcast") | Some("file_share") | Some("bot_message")
        ) && self.user.is_some()
    }

    pub fn is_dm(&self) -> bool {
        self.channel_type == "im"
    }

    pub fn conversation(&self) -> Conversation {
        match (&self.thread_ts, self.is_dm()) {
            (Some(t), true) if *t != self.ts => Conversation::thread(&self.channel, t),
            (_, true) => Conversation::dm(&self.channel),
            (Some(t), false) => Conversation::thread(&self.channel, t),
            (None, false) => Conversation::thread(&self.channel, &self.ts),
        }
    }

    pub fn is_from_bot(&self, prime_bots: &HashMap<String, String>) -> bool {
        self.bot_id.is_some()
            || self
                .user
                .as_ref()
                .is_some_and(|u| prime_bots.contains_key(u))
    }

    /// User IDs mentioned as `<@U123>` or `<@U123|name>`.
    pub fn mentions(&self) -> Vec<String> {
        let mut out = Vec::new();
        for part in self.text.split("<@").skip(1) {
            if let Some(end) = part.find('>') {
                let id = part[..end].split('|').next().unwrap_or_default();
                if !id.is_empty() && !out.iter().any(|m| m == id) {
                    out.push(id.to_string());
                }
            }
        }
        out
    }
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Deliver {
        conversation: Conversation,
        addressed_to_you: bool,
        new_conversation: bool,
    },
    Skip(&'static str),
}

/// Inputs about the receiving bot and the conversation, gathered by the daemon.
pub struct Context<'a> {
    pub self_user_id: &'a str,
    pub config: &'a BotConfig,
    /// Slack user ID -> bot name, for every live prime-bot bot (including this one).
    pub prime_bots: &'a HashMap<String, String>,
    pub participating: bool,
    /// Bot messages in a row in this conversation, counting this message.
    pub bot_streak: u32,
}

pub fn decide(msg: &Message, ctx: &Context) -> Decision {
    if !msg.is_relevant() {
        return Decision::Skip("not a user message");
    }
    let author = msg.user.as_deref().unwrap_or_default();
    if author == ctx.self_user_id {
        return Decision::Skip("own message");
    }
    if !(ctx.config.is_allowed_person(author) || ctx.prime_bots.contains_key(author)) {
        return Decision::Skip("author not allowed");
    }
    if msg.is_dm() {
        if !ctx.config.dms {
            return Decision::Skip("DMs disabled");
        }
    } else if !ctx.config.channels.contains(&msg.channel) {
        return Decision::Skip("channel not allowed");
    }
    let mentions = msg.mentions();
    let mentions_self = mentions.iter().any(|m| m == ctx.self_user_id);
    let mentions_other_bot = mentions
        .iter()
        .any(|m| m != ctx.self_user_id && ctx.prime_bots.contains_key(m));
    if !(msg.is_dm() || mentions_self || ctx.participating) {
        return Decision::Skip("not addressed");
    }
    if mentions_other_bot && !mentions_self {
        return Decision::Skip("addressed to other bots");
    }
    if msg.is_from_bot(ctx.prime_bots) && ctx.bot_streak > LOOP_LIMIT {
        return Decision::Skip("loop guard");
    }
    Decision::Deliver {
        conversation: msg.conversation(),
        addressed_to_you: mentions_self || msg.is_dm(),
        new_conversation: !ctx.participating,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "UME";

    fn cfg() -> BotConfig {
        BotConfig {
            owners: vec!["UOWN".into()],
            allowed_users: vec!["UOK".into()],
            channels: vec!["C1".into()],
            dms: true,
        }
    }

    fn bots() -> HashMap<String, String> {
        HashMap::from([
            (ME.to_string(), "atlas".to_string()),
            ("UNOVA".to_string(), "nova".to_string()),
        ])
    }

    fn msg(user: &str, text: &str) -> Message {
        Message {
            channel: "C1".into(),
            channel_type: "channel".into(),
            user: Some(user.into()),
            text: text.into(),
            ts: "2.0".into(),
            ..Default::default()
        }
    }

    fn run(m: &Message, participating: bool, streak: u32) -> Decision {
        let config = cfg();
        let prime_bots = bots();
        decide(
            m,
            &Context {
                self_user_id: ME,
                config: &config,
                prime_bots: &prime_bots,
                participating,
                bot_streak: streak,
            },
        )
    }

    #[test]
    fn mention_starts_thread_on_message() {
        let d = run(&msg("UOWN", "hi <@UME>"), false, 0);
        assert_eq!(
            d,
            Decision::Deliver {
                conversation: Conversation::thread("C1", "2.0"),
                addressed_to_you: true,
                new_conversation: true
            }
        );
    }

    #[test]
    fn participating_thread_needs_no_mention() {
        let mut m = msg("UOK", "more");
        m.thread_ts = Some("1.0".into());
        assert_eq!(run(&m, false, 0), Decision::Skip("not addressed"));
        assert_eq!(
            run(&m, true, 0),
            Decision::Deliver {
                conversation: Conversation::thread("C1", "1.0"),
                addressed_to_you: false,
                new_conversation: false
            }
        );
    }

    #[test]
    fn access_rules() {
        assert_eq!(
            run(&msg("USTRANGER", "<@UME>"), true, 0),
            Decision::Skip("author not allowed")
        );
        assert_eq!(
            run(&msg(ME, "<@UME>"), true, 0),
            Decision::Skip("own message")
        );
        let mut m = msg("UOWN", "<@UME>");
        m.channel = "C2".into();
        assert_eq!(run(&m, true, 0), Decision::Skip("channel not allowed"));
        let mut edited = msg("UOWN", "<@UME>");
        edited.subtype = Some("message_changed".into());
        assert_eq!(run(&edited, true, 0), Decision::Skip("not a user message"));
    }

    #[test]
    fn other_bot_mentions() {
        assert_eq!(
            run(&msg("UOWN", "<@UNOVA> do it"), true, 0),
            Decision::Skip("addressed to other bots")
        );
        assert!(matches!(
            run(&msg("UOWN", "<@UNOVA> <@UME|atlas>"), true, 0),
            Decision::Deliver { .. }
        ));
        // Mentioning a person who is not a bot does not exclude this bot.
        assert!(matches!(
            run(&msg("UOWN", "<@UOK> look"), true, 0),
            Decision::Deliver { .. }
        ));
    }

    #[test]
    fn bots_and_loop_guard() {
        let m = msg("UNOVA", "hey <@UME>");
        assert!(matches!(run(&m, true, 4), Decision::Deliver { .. }));
        assert_eq!(run(&m, true, 5), Decision::Skip("loop guard"));
        // Humans are never stopped by the guard.
        assert!(matches!(
            run(&msg("UOWN", "x"), true, 9),
            Decision::Deliver { .. }
        ));
    }

    #[test]
    fn dms() {
        let mut m = msg("UOK", "hello");
        m.channel = "D1".into();
        m.channel_type = "im".into();
        assert_eq!(
            run(&m, false, 0),
            Decision::Deliver {
                conversation: Conversation::dm("D1"),
                addressed_to_you: true,
                new_conversation: true
            }
        );
        m.thread_ts = Some("1.0".into());
        assert!(
            matches!(run(&m, true, 0), Decision::Deliver { conversation, .. } if conversation == Conversation::thread("D1", "1.0"))
        );
        let mut c = cfg();
        c.dms = false;
        let prime_bots = bots();
        let d = decide(
            &m,
            &Context {
                self_user_id: ME,
                config: &c,
                prime_bots: &prime_bots,
                participating: true,
                bot_streak: 0,
            },
        );
        assert_eq!(d, Decision::Skip("DMs disabled"));
    }
}
