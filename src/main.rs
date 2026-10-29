mod attachments;
mod config;
mod conversation;
mod daemon;
mod header;
mod manifest;
mod paths;
mod router;
mod rpc;
mod skill;
mod slack;
mod state;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::paths::Home;

/// Persistent Slack bot personalities backed by Prime Agent sessions.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (Slack connections and the local RPC socket).
    Daemon,
    /// Install the prime-bot Prime Agent skill.
    InstallSkill {
        /// Target directory (default: ~/.prime/agent/skills/prime-bot).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Manage bots.
    #[command(subcommand)]
    Bot(BotCommand),
    /// Manage Slack tokens.
    #[command(subcommand)]
    Secrets(SecretsCommand),
}

#[derive(Subcommand)]
enum BotCommand {
    /// Create a bot. Usually done by a Prime Agent session through the skill.
    Create {
        name: String,
        /// Slack user ID of an owner (repeatable, at least one).
        #[arg(long = "owner", required = true)]
        owners: Vec<String>,
        /// Slack user ID the bot listens to (repeatable).
        #[arg(long = "allowed-user")]
        allowed_users: Vec<String>,
        /// Slack channel ID whose messages may activate the bot (repeatable).
        #[arg(long = "channel")]
        channels: Vec<String>,
        /// Do not accept DMs.
        #[arg(long)]
        no_dms: bool,
        /// Persona text for AGENTS.md.
        #[arg(long, default_value = "")]
        persona: String,
    },
    /// Link a bot to a Prime Agent session (its durable sessionId).
    Link { name: String, session_id: String },
    /// Change a bot's message delivery config. List options replace the whole list.
    Configure {
        name: String,
        #[arg(long = "owner")]
        owners: Option<Vec<String>>,
        #[arg(long = "allowed-user")]
        allowed_users: Option<Vec<String>>,
        #[arg(long = "channel")]
        channels: Option<Vec<String>>,
        #[arg(long)]
        dms: Option<bool>,
    },
    /// Show bot status.
    Status { name: Option<String> },
    /// Print the Slack app manifest for a bot.
    Manifest { name: String },
}

#[derive(Subcommand)]
enum SecretsCommand {
    /// Set a bot's Slack tokens (prompted, hidden).
    Set { name: String },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PRIME_BOT_LOG")
                .unwrap_or_else(|_| "prime_bot=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let home = Home::from_env()?;
    let socket = home.socket();
    let result = match cli.command {
        Command::Daemon => return run_daemon(home).await,
        Command::InstallSkill { dir } => {
            let dir = match dir {
                Some(d) => d,
                None => skill::default_dir()?,
            };
            let n = skill::install(&dir)?;
            println!("installed {n} files to {}", dir.display());
            return Ok(());
        }
        Command::Bot(BotCommand::Create {
            name,
            owners,
            allowed_users,
            channels,
            no_dms,
            persona,
        }) => {
            let params = json!({
                "name": name, "owners": owners, "allowed_users": allowed_users,
                "channels": channels, "dms": !no_dms, "persona": persona,
            });
            rpc::call(&socket, "create", params).await?
        }
        Command::Bot(BotCommand::Link { name, session_id }) => {
            rpc::call(
                &socket,
                "link",
                json!({ "bot": name, "session_id": session_id }),
            )
            .await?
        }
        Command::Bot(BotCommand::Configure {
            name,
            owners,
            allowed_users,
            channels,
            dms,
        }) => {
            let params = json!({
                "bot": name, "owners": owners, "allowed_users": allowed_users, "channels": channels, "dms": dms,
            });
            rpc::call(&socket, "configure", strip_nulls(params)).await?
        }
        Command::Bot(BotCommand::Status { name }) => {
            rpc::call(&socket, "status", json!({ "bot": name })).await?
        }
        Command::Bot(BotCommand::Manifest { name }) => {
            let result = rpc::call(&socket, "manifest", json!({ "bot": name })).await?;
            print!("{}", result["manifest"].as_str().unwrap_or_default());
            return Ok(());
        }
        Command::Secrets(SecretsCommand::Set { name }) => {
            let app_token = rpassword::prompt_password("App-level token (xapp-...): ")?;
            let bot_token = rpassword::prompt_password("Bot token (xoxb-...): ")?;
            let params = json!({ "bot": name, "app_token": app_token.trim(), "bot_token": bot_token.trim() });
            rpc::call(&socket, "secrets_set", params).await?;
            println!("saved tokens for {name}");
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn strip_nulls(mut v: Value) -> Value {
    if let Some(map) = v.as_object_mut() {
        map.retain(|_, v| !v.is_null());
    }
    v
}

async fn run_daemon(home: Home) -> Result<()> {
    std::fs::create_dir_all(home.root.join("bots"))?;
    std::fs::set_permissions(&home.root, std::fs::Permissions::from_mode(0o700))?;
    let listener = rpc::bind(&home.socket()).await?;
    let daemon = Arc::new(daemon::Daemon::new(home.clone())?);
    daemon.reconcile().await;
    tokio::spawn(watch_files(daemon.clone()));
    tracing::info!("listening on {}", home.socket().display());
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = rpc::serve(listener, daemon) => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    let _ = std::fs::remove_file(home.socket());
    tracing::info!("stopped");
    Ok(())
}

/// Reloads config and secrets when either file changes on disk.
async fn watch_files(daemon: Arc<daemon::Daemon>) {
    let paths = [daemon.home.config(), daemon.home.secrets()];
    let mtimes = |paths: &[PathBuf]| -> Vec<Option<SystemTime>> {
        paths
            .iter()
            .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect()
    };
    let mut last = mtimes(&paths);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let current = mtimes(&paths);
        if current != last {
            last = current;
            daemon.reload();
            daemon.reconcile().await;
        }
    }
}
