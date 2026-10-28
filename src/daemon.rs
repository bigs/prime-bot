//! The long-running daemon: bot runtimes, event handling, delivery, RPC methods.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::task::{JoinHandle, JoinSet};

use crate::config::{self, BotConfig, BotSecrets, Config, Secrets};
use crate::conversation::Conversation;
use crate::header::{self, Header};
use crate::manifest::manifest;
use crate::paths::Home;
use crate::router::{self, Decision, Message};
use crate::slack::{self, Api};
use crate::state::State;

const AGENTS_TEMPLATE: &str = include_str!("../templates/AGENTS.md");
const NAME_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Daemon {
    pub home: Home,
    config: RwLock<Config>,
    secrets: RwLock<Secrets>,
    state: StdMutex<State>,
    bots: Mutex<HashMap<String, Running>>,
    http: reqwest::Client,
    api_base: String,
}

struct Running {
    secrets: BotSecrets,
    bot: Arc<Bot>,
    task: JoinHandle<()>,
}

pub struct Bot {
    name: String,
    api: Api,
    user_id: OnceLock<String>,
    conn: StdMutex<Conn>,
    /// Bot messages in a row per conversation, for the loop guard.
    streaks: StdMutex<HashMap<String, u32>>,
    user_names: Mutex<HashMap<String, String>>,
    channel_names: Mutex<HashMap<String, String>>,
}

#[derive(Clone)]
enum Conn {
    Connecting,
    Connected,
    Error(String),
}

struct Delivery {
    conversation: Conversation,
    channel: String,
    ts: String,
    header: Header,
    text: String,
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> RpcError {
        RpcError {
            code: -32000,
            message: format!("{e:#}"),
        }
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| RpcError {
        code: -32602,
        message: format!("invalid params: {e}"),
    })
}

fn prime_agent_bin() -> String {
    std::env::var("PRIME_BOT_PRIME_AGENT").unwrap_or_else(|_| "prime-agent".to_string())
}

async fn send_to_session(session_id: &str, text: &str) -> Result<()> {
    let out = tokio::process::Command::new(prime_agent_bin())
        .args(["send", "--json", session_id, "--", text])
        .output()
        .await
        .context("running prime-agent send")?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(if out.stderr.is_empty() {
            &out.stdout
        } else {
            &out.stderr
        });
        bail!("prime-agent send failed: {}", msg.trim());
    }
    Ok(())
}

impl Daemon {
    pub fn new(home: Home) -> Result<Daemon> {
        Ok(Daemon {
            config: RwLock::new(config::load(&home.config())?),
            secrets: RwLock::new(config::load(&home.secrets())?),
            state: StdMutex::new(State::open(&home.state())?),
            bots: Mutex::new(HashMap::new()),
            http: reqwest::Client::new(),
            api_base: slack::api_base(),
            home,
        })
    }

    fn bot_config(&self, name: &str) -> Result<BotConfig> {
        self.config
            .read()
            .unwrap()
            .bots
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown bot {name:?}"))
    }

    fn save_config(&self, cfg: &Config) -> Result<()> {
        config::save(&self.home.config(), cfg, false)
    }

    fn missing(&self, name: &str) -> Result<Vec<&'static str>> {
        let mut missing = Vec::new();
        if self.state.lock().unwrap().session(name)?.is_none() {
            missing.push("link");
        }
        let secrets = self
            .secrets
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default();
        if secrets.app_token.is_none() {
            missing.push("app_token");
        }
        if secrets.bot_token.is_none() {
            missing.push("bot_token");
        }
        Ok(missing)
    }

    /// Re-reads config and secrets from disk. Keeps the old values if a file does not parse.
    pub fn reload(&self) {
        match config::load(&self.home.config()) {
            Ok(c) => *self.config.write().unwrap() = c,
            Err(e) => tracing::error!("keeping previous config: {e:#}"),
        }
        match config::load(&self.home.secrets()) {
            Ok(s) => *self.secrets.write().unwrap() = s,
            Err(e) => tracing::error!("keeping previous secrets: {e:#}"),
        }
    }

    /// Starts bots that are linked and have tokens; stops bots that no longer qualify or whose tokens changed.
    pub async fn reconcile(self: &Arc<Self>) {
        let config = self.config.read().unwrap().clone();
        let secrets = self.secrets.read().unwrap().clone();
        let wanted: HashMap<String, BotSecrets> = config
            .bots
            .keys()
            .filter(|name| self.missing(name).map(|m| m.is_empty()).unwrap_or(false))
            .map(|name| (name.clone(), secrets.get(name).cloned().unwrap_or_default()))
            .collect();
        let mut bots = self.bots.lock().await;
        bots.retain(|name, running| {
            let keep = wanted.get(name) == Some(&running.secrets);
            if !keep {
                tracing::info!(bot = %name, "stopping");
                running.task.abort();
            }
            keep
        });
        for (name, secrets) in wanted {
            if bots.contains_key(&name) {
                continue;
            }
            tracing::info!(bot = %name, "starting");
            let bot = Arc::new(Bot {
                name: name.clone(),
                api: Api::new(
                    self.http.clone(),
                    self.api_base.clone(),
                    secrets.bot_token.clone().unwrap_or_default(),
                ),
                user_id: OnceLock::new(),
                conn: StdMutex::new(Conn::Connecting),
                streaks: StdMutex::new(HashMap::new()),
                user_names: Mutex::new(HashMap::new()),
                channel_names: Mutex::new(HashMap::new()),
            });
            let app_token = secrets.app_token.clone().unwrap_or_default();
            let task = tokio::spawn(self.clone().run_bot(bot.clone(), app_token));
            bots.insert(name, Running { secrets, bot, task });
        }
    }

    async fn run_bot(self: Arc<Self>, bot: Arc<Bot>, app_token: String) {
        let mut backoff = 1;
        while bot.user_id.get().is_none() {
            match bot.api.call("auth.test", &[]).await {
                Ok(body) => {
                    let _ = bot
                        .user_id
                        .set(body["user_id"].as_str().unwrap_or_default().to_string());
                }
                Err(e) => {
                    tracing::error!(bot = %bot.name, "auth.test: {e:#}");
                    *bot.conn.lock().unwrap() = Conn::Error(format!("{e:#}"));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
        // Tasks in the set are aborted when this future is aborted.
        let mut tasks = JoinSet::new();
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(256);
        let (delivery_tx, mut delivery_rx) = mpsc::channel::<Delivery>(256);
        let (d, b) = (self.clone(), bot.clone());
        tasks.spawn(async move {
            while let Some(payload) = event_rx.recv().await {
                if let Err(e) = d.handle_event(&b, payload, &delivery_tx).await {
                    tracing::error!(bot = %b.name, "event: {e:#}");
                }
            }
        });
        let (d, b) = (self.clone(), bot.clone());
        tasks.spawn(async move {
            while let Some(delivery) = delivery_rx.recv().await {
                d.deliver(&b, delivery).await;
            }
        });
        let mut backoff = 1;
        loop {
            *bot.conn.lock().unwrap() = Conn::Connecting;
            let on_hello = || {
                tracing::info!(bot = %bot.name, "connected");
                *bot.conn.lock().unwrap() = Conn::Connected;
            };
            match slack::run_socket(&self.http, &self.api_base, &app_token, &event_tx, on_hello)
                .await
            {
                Ok(()) => backoff = 1,
                Err(e) => {
                    tracing::error!(bot = %bot.name, "socket: {e:#}");
                    *bot.conn.lock().unwrap() = Conn::Error(format!("{e:#}"));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    }

    /// Slack user ID -> bot name for every running bot that knows its identity.
    async fn prime_bots(&self) -> HashMap<String, String> {
        self.bots
            .lock()
            .await
            .values()
            .filter_map(|r| {
                r.bot
                    .user_id
                    .get()
                    .map(|id| (id.clone(), r.bot.name.clone()))
            })
            .collect()
    }

    async fn handle_event(
        &self,
        bot: &Arc<Bot>,
        payload: Value,
        deliveries: &mpsc::Sender<Delivery>,
    ) -> Result<()> {
        if let Some(id) = payload["event_id"].as_str()
            && self
                .state
                .lock()
                .unwrap()
                .seen_event(&bot.name, id, now())?
        {
            return Ok(());
        }
        let event = &payload["event"];
        match event["type"].as_str() {
            Some("message") => self.handle_message(bot, event, deliveries).await,
            Some("member_joined_channel") => self.handle_join(bot, event).await,
            _ => Ok(()),
        }
    }

    async fn handle_message(
        &self,
        bot: &Arc<Bot>,
        event: &Value,
        deliveries: &mpsc::Sender<Delivery>,
    ) -> Result<()> {
        let msg = Message::from_event(event);
        if !msg.is_relevant() {
            return Ok(());
        }
        let Ok(config) = self.bot_config(&bot.name) else {
            return Ok(());
        };
        let self_id = bot.user_id.get().cloned().unwrap_or_default();
        let prime_bots = self.prime_bots().await;
        let conv = msg.conversation();
        let bot_streak = {
            let mut streaks = bot.streaks.lock().unwrap();
            let streak = streaks.entry(conv.to_string()).or_insert(0);
            *streak = if msg.is_from_bot(&prime_bots) {
                *streak + 1
            } else {
                0
            };
            *streak
        };
        let at: f64 = msg.ts.parse().unwrap_or_else(|_| now());
        let participating = {
            let state = self.state.lock().unwrap();
            let p = state.is_participating(&bot.name, &conv)?;
            if p {
                state.touch(&bot.name, &conv, false, at)?;
            }
            p
        };
        let ctx = router::Context {
            self_user_id: &self_id,
            config: &config,
            prime_bots: &prime_bots,
            participating,
            bot_streak,
        };
        let (conversation, addressed_to_you, new_conversation) = match router::decide(&msg, &ctx) {
            Decision::Skip(reason) => {
                tracing::debug!(bot = %bot.name, conversation = %conv, "skip: {reason}");
                return Ok(());
            }
            Decision::Deliver {
                conversation,
                addressed_to_you,
                new_conversation,
            } => (conversation, addressed_to_you, new_conversation),
        };
        let other_bots = {
            let state = self.state.lock().unwrap();
            state.touch(&bot.name, &conversation, true, at)?;
            state.other_participants(&bot.name, &conversation)?
        };
        let author = msg.user.clone().unwrap_or_default();
        let author_name = async {
            match prime_bots.get(&author) {
                Some(name) => name.clone(),
                None => self.user_name(bot, &author).await,
            }
        };
        let (author_name, channel) =
            tokio::join!(author_name, self.channel_label(bot, &msg.channel));
        let role = if prime_bots.contains_key(&author) {
            "prime-bot"
        } else if config.is_owner(&author) {
            "owner"
        } else {
            "allowed"
        };
        let from = format!("{author_name} ({author}, {role})");
        let user_names = bot.user_names.lock().await;
        let mentions = msg
            .mentions()
            .into_iter()
            .map(
                |id| match prime_bots.get(&id).or_else(|| user_names.get(&id)) {
                    Some(name) => format!("{name} ({id})"),
                    None => id,
                },
            )
            .collect();
        drop(user_names);
        let header = Header {
            bot: bot.name.clone(),
            conversation: conversation.to_string(),
            msg_ts: msg.ts.clone(),
            channel,
            kind: conversation.kind(),
            new_conversation,
            from,
            mentions,
            addressed_to_you,
            other_bots,
            note: None,
        };
        let delivery = Delivery {
            conversation,
            channel: msg.channel.clone(),
            ts: msg.ts.clone(),
            header,
            text: msg.text,
        };
        deliveries
            .send(delivery)
            .await
            .map_err(|_| anyhow!("delivery queue closed"))?;
        Ok(())
    }

    async fn deliver(&self, bot: &Arc<Bot>, mut d: Delivery) {
        let (route, root) = {
            let state = self.state.lock().unwrap();
            (
                state.route(&bot.name, &d.conversation).ok().flatten(),
                state.session(&bot.name).ok().flatten(),
            )
        };
        let mut delivered = false;
        if let Some(target) = route {
            match send_to_session(&target, &header::render(&d.header, &d.text)).await {
                Ok(()) => delivered = true,
                Err(e) => {
                    tracing::warn!(bot = %bot.name, conversation = %d.conversation, "bound session {target} failed: {e:#}");
                    let _ = self
                        .state
                        .lock()
                        .unwrap()
                        .unbind(&bot.name, &d.conversation);
                    d.header.note = Some(format!(
                        "the session bound to this conversation ({target}) failed, so the binding was removed"
                    ));
                }
            }
        }
        if !delivered {
            match root {
                Some(root) => match send_to_session(&root, &header::render(&d.header, &d.text))
                    .await
                {
                    Ok(()) => delivered = true,
                    Err(e) => tracing::error!(bot = %bot.name, "delivery to {root} failed: {e:#}"),
                },
                None => tracing::error!(bot = %bot.name, "not linked to a session"),
            }
        }
        let emoji = if delivered { "eyes" } else { "warning" };
        if let Err(e) = bot
            .api
            .call(
                "reactions.add",
                &[
                    ("channel", &d.channel),
                    ("timestamp", &d.ts),
                    ("name", emoji),
                ],
            )
            .await
        {
            tracing::warn!(bot = %bot.name, "reaction: {e:#}");
        }
    }

    async fn handle_join(&self, bot: &Arc<Bot>, event: &Value) -> Result<()> {
        if event["user"].as_str() != bot.user_id.get().map(String::as_str) {
            return Ok(());
        }
        let channel = event["channel"].as_str().unwrap_or_default().to_string();
        let inviter = event["inviter"].as_str().unwrap_or_default();
        {
            let mut cfg = self.config.write().unwrap();
            let Some(bc) = cfg.bots.get_mut(&bot.name) else {
                return Ok(());
            };
            if bc.channels.contains(&channel) {
                return Ok(());
            }
            if bc.is_owner(inviter) {
                bc.channels.push(channel.clone());
                self.save_config(&cfg)?;
                tracing::info!(bot = %bot.name, %channel, %inviter, "invited by owner; channel added");
                return Ok(());
            }
        }
        tracing::warn!(bot = %bot.name, %channel, %inviter, "not invited by an owner; leaving");
        bot.api
            .call("conversations.leave", &[("channel", &channel)])
            .await?;
        Ok(())
    }

    async fn user_name(&self, bot: &Bot, user_id: &str) -> String {
        if let Some(n) = bot.user_names.lock().await.get(user_id) {
            return n.clone();
        }
        let lookup = tokio::time::timeout(
            NAME_LOOKUP_TIMEOUT,
            bot.api.call("users.info", &[("user", user_id)]),
        )
        .await;
        match lookup {
            Ok(Ok(body)) => {
                let u = &body["user"];
                let name = [&u["profile"]["display_name"], &u["real_name"], &u["name"]]
                    .iter()
                    .filter_map(|v| v.as_str())
                    .find(|s| !s.is_empty())
                    .unwrap_or(user_id)
                    .to_string();
                bot.user_names
                    .lock()
                    .await
                    .insert(user_id.to_string(), name.clone());
                name
            }
            Ok(Err(e)) => {
                tracing::warn!(bot = %bot.name, "users.info {user_id}: {e:#}");
                user_id.to_string()
            }
            Err(_) => {
                tracing::warn!(bot = %bot.name, "users.info {user_id}: name lookup timed out");
                user_id.to_string()
            }
        }
    }

    async fn channel_label(&self, bot: &Bot, channel: &str) -> String {
        if config::is_dm_channel(channel) {
            return format!("DM ({channel})");
        }
        if let Some(n) = bot.channel_names.lock().await.get(channel) {
            return format!("#{n} ({channel})");
        }
        let lookup = tokio::time::timeout(
            NAME_LOOKUP_TIMEOUT,
            bot.api.call("conversations.info", &[("channel", channel)]),
        )
        .await;
        match lookup {
            Ok(Ok(body)) => {
                let name = body["channel"]["name"]
                    .as_str()
                    .unwrap_or(channel)
                    .to_string();
                bot.channel_names
                    .lock()
                    .await
                    .insert(channel.to_string(), name.clone());
                format!("#{name} ({channel})")
            }
            Ok(Err(e)) => {
                tracing::warn!(bot = %bot.name, "conversations.info {channel}: {e:#}");
                channel.to_string()
            }
            Err(_) => {
                tracing::warn!(bot = %bot.name, "conversations.info {channel}: name lookup timed out");
                channel.to_string()
            }
        }
    }

    async fn live(&self, name: &str) -> Result<Arc<Bot>> {
        self.bot_config(name)?;
        let bots = self.bots.lock().await;
        let Some(running) = bots.get(name) else {
            bail!(
                "bot {name:?} is not live; missing: {}",
                self.missing(name)?.join(", ")
            );
        };
        if running.bot.user_id.get().is_none() {
            bail!("bot {name:?} is still connecting to Slack");
        }
        Ok(running.bot.clone())
    }

    pub async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "create" => self.create(parse(params)?),
            "link" => self.link(parse(params)?).await,
            "configure" => self.configure(parse(params)?),
            "status" => self.status(parse(params)?).await,
            "manifest" => {
                let p: BotParam = parse(params)?;
                self.bot_config(&p.bot)?;
                Ok(json!({ "manifest": manifest(&p.bot) }))
            }
            "secrets_set" => self.secrets_set(parse(params)?).await,
            "reply" => self.reply(parse(params)?).await,
            "post" => self.post(parse(params)?).await,
            "thread_history" => self.thread_history(parse(params)?).await,
            "list_threads" => self.list_threads(parse(params)?),
            "react" => self.react(parse(params)?).await,
            "bind_thread" => self.bind_thread(parse(params)?),
            "unbind_thread" => self.unbind_thread(parse(params)?),
            "user_info" => self.user_info(parse(params)?).await,
            _ => Err(RpcError {
                code: -32601,
                message: format!("unknown method {method:?}"),
            }),
        }
    }

    fn create(&self, p: CreateParams) -> Result<Value, RpcError> {
        config::validate_name(&p.name)?;
        if p.owners.is_empty() {
            return Err(anyhow!("a bot needs at least one owner").into());
        }
        let dir = self.home.bot_dir(&p.name);
        let mut cfg = self.config.write().unwrap();
        if cfg.bots.contains_key(&p.name) {
            return Err(anyhow!("bot {:?} already exists", p.name).into());
        }
        if dir.exists() {
            return Err(anyhow!("{} already exists", dir.display()).into());
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let git = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&dir)
            .output();
        match git {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                return Err(anyhow!(
                    "git init failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )
                .into());
            }
            Err(e) => return Err(anyhow!("running git: {e}").into()),
        }
        let agents_md = dir.join("AGENTS.md");
        let text = AGENTS_TEMPLATE
            .replace("{{name}}", &p.name)
            .replace("{{persona}}", p.persona.trim());
        std::fs::write(&agents_md, text).context("writing AGENTS.md")?;
        cfg.bots.insert(
            p.name.clone(),
            BotConfig {
                owners: p.owners,
                allowed_users: p.allowed_users.unwrap_or_default(),
                channels: p.channels.unwrap_or_default(),
                dms: p.dms.unwrap_or(true),
            },
        );
        self.save_config(&cfg)?;
        let mut secrets = self.secrets.write().unwrap();
        secrets.entry(p.name.clone()).or_default();
        config::save(&self.home.secrets(), &*secrets, true)?;
        Ok(json!({
            "bot": p.name,
            "dir": dir,
            "agents_md_path": agents_md,
            "manifest": manifest(&p.name),
            "secrets_command": format!("prime-bot secrets set {}", p.name),
            "session_name": format!("prime-bot-{}", p.name),
        }))
    }

    async fn link(self: &Arc<Self>, p: LinkParams) -> Result<Value, RpcError> {
        self.bot_config(&p.bot)?;
        if p.session_id.trim().is_empty() {
            return Err(anyhow!("session_id is empty").into());
        }
        self.state.lock().unwrap().link(&p.bot, &p.session_id)?;
        self.reconcile().await;
        Ok(json!({ "bot": p.bot, "session_id": p.session_id }))
    }

    fn configure(&self, p: ConfigureParams) -> Result<Value, RpcError> {
        let mut cfg = self.config.write().unwrap();
        let bc = cfg
            .bots
            .get_mut(&p.bot)
            .ok_or_else(|| anyhow!("unknown bot {:?}", p.bot))?;
        if let Some(owners) = p.owners {
            if owners.is_empty() {
                return Err(anyhow!("a bot needs at least one owner").into());
            }
            bc.owners = owners;
        }
        if let Some(v) = p.allowed_users {
            bc.allowed_users = v;
        }
        if let Some(v) = p.channels {
            bc.channels = v;
        }
        if let Some(v) = p.dms {
            bc.dms = v;
        }
        let result = serde_json::to_value(&*bc).map_err(anyhow::Error::from)?;
        self.save_config(&cfg)?;
        Ok(result)
    }

    async fn status(&self, p: OptionalBotParam) -> Result<Value, RpcError> {
        let config = self.config.read().unwrap().clone();
        if let Some(name) = &p.bot {
            self.bot_config(name)?;
        }
        let bots = self.bots.lock().await;
        let mut out = Vec::new();
        for (name, bc) in &config.bots {
            if p.bot.as_ref().is_some_and(|b| b != name) {
                continue;
            }
            let secrets = self
                .secrets
                .read()
                .unwrap()
                .get(name)
                .cloned()
                .unwrap_or_default();
            let (connection, error, user_id) = match bots.get(name) {
                None => ("not_live", None, None),
                Some(r) => {
                    let user_id = r.bot.user_id.get().cloned();
                    match r.bot.conn.lock().unwrap().clone() {
                        Conn::Connecting => ("connecting", None, user_id),
                        Conn::Connected => ("connected", None, user_id),
                        Conn::Error(e) => ("error", Some(e), user_id),
                    }
                }
            };
            out.push(json!({
                "name": name,
                "dir": self.home.bot_dir(name),
                "session_id": self.state.lock().unwrap().session(name)?,
                "tokens": { "app_token": secrets.app_token.is_some(), "bot_token": secrets.bot_token.is_some() },
                "connection": connection,
                "error": error,
                "slack_user_id": user_id,
                "owners": bc.owners,
                "allowed_users": bc.allowed_users,
                "channels": bc.channels,
                "dms": bc.dms,
                "missing": self.missing(name)?,
            }));
        }
        Ok(json!({ "bots": out }))
    }

    async fn secrets_set(self: &Arc<Self>, p: SecretsParams) -> Result<Value, RpcError> {
        self.bot_config(&p.bot)?;
        if !p.app_token.starts_with("xapp-") {
            return Err(anyhow!("the app-level token must start with xapp-").into());
        }
        if !p.bot_token.starts_with("xoxb-") {
            return Err(anyhow!("the bot token must start with xoxb-").into());
        }
        {
            let mut secrets = self.secrets.write().unwrap();
            secrets.insert(
                p.bot.clone(),
                BotSecrets {
                    app_token: Some(p.app_token),
                    bot_token: Some(p.bot_token),
                },
            );
            config::save(&self.home.secrets(), &*secrets, true)?;
        }
        self.reconcile().await;
        Ok(json!({ "bot": p.bot }))
    }

    async fn reply(&self, p: ReplyParams) -> Result<Value, RpcError> {
        let conv = Conversation::parse(&p.conversation)?;
        self.post(PostParams {
            bot: p.bot,
            channel: conv.channel,
            text: p.text,
            thread_ts: conv.thread_ts,
        })
        .await
    }

    async fn post(&self, p: PostParams) -> Result<Value, RpcError> {
        let bot = self.live(&p.bot).await?;
        let ts = bot
            .api
            .post_message(&p.channel, &p.text, p.thread_ts.as_deref())
            .await?;
        let conv = match &p.thread_ts {
            Some(t) => Conversation::thread(&p.channel, t),
            None if config::is_dm_channel(&p.channel) => Conversation::dm(&p.channel),
            None => Conversation::thread(&p.channel, &ts),
        };
        self.state
            .lock()
            .unwrap()
            .touch(&p.bot, &conv, true, now())?;
        Ok(json!({ "channel": p.channel, "ts": ts, "conversation": conv.to_string() }))
    }

    async fn thread_history(&self, p: HistoryParams) -> Result<Value, RpcError> {
        let bot = self.live(&p.bot).await?;
        let config = self.bot_config(&p.bot)?;
        let conv = Conversation::parse(&p.conversation)?;
        let limit = p.limit.unwrap_or(50).clamp(1, 200).to_string();
        let mut params = vec![
            ("channel", conv.channel.as_str()),
            ("limit", limit.as_str()),
        ];
        if let Some(c) = p.cursor.as_deref() {
            params.push(("cursor", c));
        }
        let body = match &conv.thread_ts {
            Some(ts) => {
                params.push(("ts", ts));
                bot.api.call("conversations.replies", &params).await?
            }
            None => bot.api.call("conversations.history", &params).await?,
        };
        let mut raw: Vec<Value> = body["messages"].as_array().cloned().unwrap_or_default();
        if conv.thread_ts.is_none() {
            raw.reverse();
        }
        let prime_bots = self.prime_bots().await;
        let mut messages = Vec::new();
        for m in raw {
            let user = m["user"].as_str().map(str::to_string);
            let bot_name = user.as_ref().and_then(|u| prime_bots.get(u)).cloned();
            let user_name = match (&user, &bot_name) {
                (_, Some(n)) => Some(n.clone()),
                (Some(u), None) => Some(self.user_name(&bot, u).await),
                _ => None,
            };
            let allowed =
                bot_name.is_some() || user.as_deref().is_some_and(|u| config.is_allowed_person(u));
            messages.push(json!({
                "ts": m["ts"],
                "thread_ts": m["thread_ts"],
                "user": user,
                "user_name": user_name,
                "is_bot": m.get("bot_id").is_some() || bot_name.is_some(),
                "bot_name": bot_name,
                "allowed": allowed,
                "text": m["text"],
            }));
        }
        let next = body["response_metadata"]["next_cursor"]
            .as_str()
            .filter(|c| !c.is_empty());
        Ok(json!({ "messages": messages, "next_cursor": next }))
    }

    fn list_threads(&self, p: ListThreadsParams) -> Result<Value, RpcError> {
        self.bot_config(&p.bot)?;
        let rows = self
            .state
            .lock()
            .unwrap()
            .list_threads(&p.bot, p.active_since)?;
        let threads: Vec<Value> = rows
            .into_iter()
            .map(|r| {
                json!({
                    "conversation": r.conversation,
                    "channel": r.channel,
                    "thread_ts": r.thread_ts,
                    "kind": r.kind,
                    "last_activity": r.last_activity,
                    "route": r.route,
                })
            })
            .collect();
        Ok(json!({ "threads": threads }))
    }

    async fn react(&self, p: ReactParams) -> Result<Value, RpcError> {
        let bot = self.live(&p.bot).await?;
        let method = if p.remove.unwrap_or(false) {
            "reactions.remove"
        } else {
            "reactions.add"
        };
        let emoji = p.emoji.trim_matches(':');
        bot.api
            .call(
                method,
                &[
                    ("channel", &p.channel),
                    ("timestamp", &p.ts),
                    ("name", emoji),
                ],
            )
            .await?;
        Ok(json!({}))
    }

    fn bind_thread(&self, p: BindParams) -> Result<Value, RpcError> {
        self.bot_config(&p.bot)?;
        let conv = Conversation::parse(&p.conversation)?;
        if p.session_id.trim().is_empty() {
            return Err(anyhow!("session_id is empty").into());
        }
        self.state
            .lock()
            .unwrap()
            .bind(&p.bot, &conv, &p.session_id)?;
        Ok(json!({}))
    }

    fn unbind_thread(&self, p: ConversationParams) -> Result<Value, RpcError> {
        self.bot_config(&p.bot)?;
        let conv = Conversation::parse(&p.conversation)?;
        self.state.lock().unwrap().unbind(&p.bot, &conv)?;
        Ok(json!({}))
    }

    async fn user_info(&self, p: UserInfoParams) -> Result<Value, RpcError> {
        let bot = self.live(&p.bot).await?;
        let body = bot.api.call("users.info", &[("user", &p.user_id)]).await?;
        let u = &body["user"];
        Ok(json!({
            "id": u["id"],
            "name": u["name"],
            "real_name": u["real_name"],
            "display_name": u["profile"]["display_name"],
            "is_bot": u["is_bot"].as_bool().unwrap_or(false),
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateParams {
    name: String,
    owners: Vec<String>,
    allowed_users: Option<Vec<String>>,
    channels: Option<Vec<String>>,
    dms: Option<bool>,
    persona: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkParams {
    bot: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureParams {
    bot: String,
    owners: Option<Vec<String>>,
    allowed_users: Option<Vec<String>>,
    channels: Option<Vec<String>>,
    dms: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BotParam {
    bot: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalBotParam {
    bot: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretsParams {
    bot: String,
    app_token: String,
    bot_token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyParams {
    bot: String,
    conversation: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostParams {
    bot: String,
    channel: String,
    text: String,
    thread_ts: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryParams {
    bot: String,
    conversation: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListThreadsParams {
    bot: String,
    active_since: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactParams {
    bot: String,
    channel: String,
    ts: String,
    emoji: String,
    remove: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindParams {
    bot: String,
    conversation: String,
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationParams {
    bot: String,
    conversation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserInfoParams {
    bot: String,
    user_id: String,
}
