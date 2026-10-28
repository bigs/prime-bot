//! The text delivered to a bot session for one Slack message.

#[derive(Clone, Debug)]
pub struct Header {
    pub bot: String,
    pub conversation: String,
    pub msg_ts: String,
    /// `#name (C123)` or `DM (D123)`.
    pub channel: String,
    pub kind: &'static str,
    pub new_conversation: bool,
    /// `Alice (U123, owner)`.
    pub from: String,
    /// `atlas (U1)`, `Bob (U2)`.
    pub mentions: Vec<String>,
    pub addressed_to_you: bool,
    pub other_bots: Vec<String>,
    pub note: Option<String>,
}

pub fn render(h: &Header, text: &str) -> String {
    let mut out = format!(
        "[prime-bot] bot={} conversation={} msg={}\nchannel={} kind={} new_conversation={}\nfrom={}\nmentions=[{}] addressed_to_you={} other_bots_in_thread=[{}]\n",
        h.bot,
        h.conversation,
        h.msg_ts,
        h.channel,
        h.kind,
        h.new_conversation,
        h.from,
        h.mentions.join(", "),
        h.addressed_to_you,
        h.other_bots.join(", "),
    );
    if let Some(note) = &h.note {
        out.push_str(&format!("note={note}\n"));
    }
    out.push_str("---\n");
    out.push_str(text);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders() {
        let h = Header {
            bot: "atlas".into(),
            conversation: "C1/1.0".into(),
            msg_ts: "2.0".into(),
            channel: "#eng (C1)".into(),
            kind: "thread",
            new_conversation: false,
            from: "Alice (U9, owner)".into(),
            mentions: vec!["atlas (U1)".into()],
            addressed_to_you: true,
            other_bots: vec!["nova".into()],
            note: None,
        };
        assert_eq!(
            render(&h, "hi"),
            "[prime-bot] bot=atlas conversation=C1/1.0 msg=2.0\nchannel=#eng (C1) kind=thread new_conversation=false\nfrom=Alice (U9, owner)\nmentions=[atlas (U1)] addressed_to_you=true other_bots_in_thread=[nova]\n---\nhi"
        );
    }
}
