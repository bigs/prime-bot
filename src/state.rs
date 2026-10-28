use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::conversation::Conversation;

/// Daemon-owned state: session links, conversations, routes, seen events.
pub struct State {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRow {
    pub conversation: String,
    pub channel: String,
    pub thread_ts: Option<String>,
    pub kind: String,
    pub last_activity: f64,
    pub route: Option<String>,
}

impl State {
    pub fn open(path: &Path) -> Result<State> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS links (bot TEXT PRIMARY KEY, session_id TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS conversations (
                bot TEXT NOT NULL, conversation TEXT NOT NULL, channel TEXT NOT NULL, thread_ts TEXT,
                participating INTEGER NOT NULL, last_activity REAL NOT NULL,
                PRIMARY KEY (bot, conversation));
             CREATE TABLE IF NOT EXISTS routes (
                bot TEXT NOT NULL, conversation TEXT NOT NULL, session_id TEXT NOT NULL,
                PRIMARY KEY (bot, conversation));
             CREATE TABLE IF NOT EXISTS events (
                bot TEXT NOT NULL, event_id TEXT NOT NULL, received_at REAL NOT NULL,
                PRIMARY KEY (bot, event_id));",
        )?;
        Ok(State { conn })
    }

    pub fn link(&self, bot: &str, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO links (bot, session_id) VALUES (?1, ?2)
             ON CONFLICT (bot) DO UPDATE SET session_id = excluded.session_id",
            params![bot, session_id],
        )?;
        Ok(())
    }

    pub fn session(&self, bot: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT session_id FROM links WHERE bot = ?1", [bot], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Records activity. Participation is sticky: once set it stays set.
    pub fn touch(
        &self,
        bot: &str,
        conv: &Conversation,
        participating: bool,
        at: f64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conversations (bot, conversation, channel, thread_ts, participating, last_activity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (bot, conversation) DO UPDATE SET
                participating = MAX(participating, excluded.participating),
                last_activity = MAX(last_activity, excluded.last_activity)",
            params![bot, conv.to_string(), conv.channel, conv.thread_ts, participating as i64, at],
        )?;
        Ok(())
    }

    pub fn is_participating(&self, bot: &str, conv: &Conversation) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT participating FROM conversations WHERE bot = ?1 AND conversation = ?2",
                params![bot, conv.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found == Some(1))
    }

    /// Bots (other than `bot`) participating in the conversation.
    pub fn other_participants(&self, bot: &str, conv: &Conversation) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT bot FROM conversations WHERE conversation = ?1 AND bot != ?2 AND participating = 1 ORDER BY bot",
        )?;
        let rows = stmt.query_map(params![conv.to_string(), bot], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_threads(&self, bot: &str, active_since: Option<f64>) -> Result<Vec<ThreadRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.conversation, c.channel, c.thread_ts, c.last_activity, r.session_id
             FROM conversations c LEFT JOIN routes r ON r.bot = c.bot AND r.conversation = c.conversation
             WHERE c.bot = ?1 AND c.participating = 1 AND c.last_activity >= ?2
             ORDER BY c.last_activity DESC",
        )?;
        let rows = stmt.query_map(params![bot, active_since.unwrap_or(0.0)], |r| {
            let thread_ts: Option<String> = r.get(2)?;
            Ok(ThreadRow {
                conversation: r.get(0)?,
                channel: r.get(1)?,
                kind: if thread_ts.is_some() {
                    "thread".into()
                } else {
                    "dm".into()
                },
                thread_ts,
                last_activity: r.get(3)?,
                route: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn route(&self, bot: &str, conv: &Conversation) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id FROM routes WHERE bot = ?1 AND conversation = ?2",
                params![bot, conv.to_string()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn bind(&self, bot: &str, conv: &Conversation, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO routes (bot, conversation, session_id) VALUES (?1, ?2, ?3)
             ON CONFLICT (bot, conversation) DO UPDATE SET session_id = excluded.session_id",
            params![bot, conv.to_string(), session_id],
        )?;
        Ok(())
    }

    pub fn unbind(&self, bot: &str, conv: &Conversation) -> Result<()> {
        self.conn.execute(
            "DELETE FROM routes WHERE bot = ?1 AND conversation = ?2",
            params![bot, conv.to_string()],
        )?;
        Ok(())
    }

    /// Returns true if the event was already seen. Forgets events older than a day.
    pub fn seen_event(&self, bot: &str, event_id: &str, now: f64) -> Result<bool> {
        self.conn.execute(
            "DELETE FROM events WHERE received_at < ?1",
            [now - 86_400.0],
        )?;
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO events (bot, event_id, received_at) VALUES (?1, ?2, ?3)",
            params![bot, event_id, now],
        )?;
        Ok(inserted == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let s = State::open(&dir.path().join("state.sqlite")).unwrap();
        (dir, s)
    }

    #[test]
    fn links() {
        let (_d, s) = state();
        assert_eq!(s.session("atlas").unwrap(), None);
        s.link("atlas", "a").unwrap();
        s.link("atlas", "b").unwrap();
        assert_eq!(s.session("atlas").unwrap().as_deref(), Some("b"));
    }

    #[test]
    fn participation_is_sticky_and_listed() {
        let (_d, s) = state();
        let c = Conversation::thread("C1", "1.0");
        s.touch("atlas", &c, false, 1.0).unwrap();
        assert!(!s.is_participating("atlas", &c).unwrap());
        s.touch("atlas", &c, true, 2.0).unwrap();
        s.touch("atlas", &c, false, 3.0).unwrap();
        assert!(s.is_participating("atlas", &c).unwrap());
        s.touch("nova", &c, true, 3.0).unwrap();
        assert_eq!(
            s.other_participants("atlas", &c).unwrap(),
            vec!["nova".to_string()]
        );
        let dm = Conversation::dm("D1");
        s.touch("atlas", &dm, true, 5.0).unwrap();
        s.bind("atlas", &c, "child").unwrap();
        let rows = s.list_threads("atlas", None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].conversation, "D1");
        assert_eq!(rows[0].kind, "dm");
        assert_eq!(rows[1].route.as_deref(), Some("child"));
        assert_eq!(rows[1].last_activity, 3.0);
        assert_eq!(s.list_threads("atlas", Some(4.0)).unwrap().len(), 1);
    }

    #[test]
    fn routes() {
        let (_d, s) = state();
        let c = Conversation::thread("C1", "1.0");
        s.bind("atlas", &c, "x").unwrap();
        assert_eq!(s.route("atlas", &c).unwrap().as_deref(), Some("x"));
        s.unbind("atlas", &c).unwrap();
        assert_eq!(s.route("atlas", &c).unwrap(), None);
    }

    #[test]
    fn events_dedupe() {
        let (_d, s) = state();
        assert!(!s.seen_event("atlas", "E1", 100.0).unwrap());
        assert!(s.seen_event("atlas", "E1", 101.0).unwrap());
        assert!(!s.seen_event("nova", "E1", 101.0).unwrap());
        assert!(!s.seen_event("atlas", "E1", 100.0 + 90_000.0).unwrap());
    }
}
