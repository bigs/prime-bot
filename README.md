# prime-bot

Give your [Prime Agent](https://primeintellect.ai) sessions a home in Slack. Create a bot with a name and a persona, then talk to it in DMs or channel threads. It keeps a persistent agent session and its own working directory, so it can continue work across conversations instead of starting from scratch on every message.

You can run several personalities side by side: a release engineer, a research assistant, or a teammate for a particular project. Each has its own Slack app. Bots can share a thread or delegate a thread to a persistent sub-agent.

You run one local daemon and ask a Prime Agent root session to create and manage the bots. You create the Slack apps and enter their tokens in your terminal—not in agent chat. Slack uses Socket Mode, so there is no public webhook or inbound HTTP server to expose.

**Intended for trusted local deployments.** Bot sessions share the authority of the Unix account running them. This is not a sandbox or a multi-tenant service. See [Trust and permissions](#trust-and-permissions).

## Quick start

### Prerequisites

- A Unix host. The implementation uses Unix sockets and signals; it has been tested on Linux.
- A current stable Rust toolchain and Git to build and install from source.
- A working Prime Agent installation, including `prime-agent` on the daemon's `PATH` and access to root sessions that can call `rlm.create_session`.
- Python 3.10+ for the installed skill.
- A Slack workspace where you can create and install apps, or get an administrator's approval.

### 1. Install and start the bridge

From this checkout:

```sh
cargo install --path . --locked
prime-bot install-skill
prime-bot daemon
```

Cargo installs the binary into `~/.cargo/bin` by default; ensure that directory is on your `PATH`. Leave the daemon running in this terminal, or manage it with your own process supervisor. Run one prime-bot daemon per home directory.

`install-skill` installs the bundled skill at `~/.prime/agent/skills/prime-bot/`. Open a **fresh Prime Agent root session** afterward so it discovers the skill. You do not need to restart the Prime Agent daemon.

### 2. Ask the agent to create a bot

For example:

> Create a Slack bot called atlas. It is a concise release engineer who helps investigate failed builds. Its owner is Slack user ID U012ABC.

Replace the example user ID with yours: in Slack, open your profile, choose the more-actions menu, and select **Copy member ID**.

The agent creates the bot's files, gives you a Slack app manifest, creates a separate root session for the bot, and links it to the bridge. You can also supply other allowed Slack users and channels.

### 3. Create the Slack app and enter its tokens

1. Open [Slack's app dashboard](https://api.slack.com/apps) and choose **Create New App → From a manifest**. Use the manifest supplied by the agent.
2. Under **Basic Information**, generate an app-level token with the `connections:write` scope.
3. Install the app to your workspace. Copy its **Bot User OAuth Token** from **OAuth & Permissions**.
4. In a separate terminal, run:

   ```sh
   prime-bot secrets set atlas
   ```

   Enter the app-level token (`xapp-...`) and bot token (`xoxb-...`) at the hidden prompts. Never paste tokens into agent chat or commit them.

The bot connects once it has both tokens and a linked session, in either order. No bridge restart is needed.

### 4. Talk to it

```sh
prime-bot bot status atlas
```

Look for `connection: "connected"` and an empty `missing` list. DM the bot as its owner, or invite it to a channel as its owner and @mention it. Owner invitations automatically add the channel to its incoming-message subscriptions.

A mention starts a conversation. Later replies in that thread can reach the bot without another mention. A normal Prime Agent final answer is **not** posted to Slack: the bot must call `prime_bot.reply()` or `prime_bot.post()`.

## Instructions for agents

### Choose your role

- **Installing or managing bots:** read [skill/SKILL.md](skill/SKILL.md), or its installed copy at `~/.prime/agent/skills/prime-bot/SKILL.md`.
- **Acting as a bot:** read your working directory's `AGENTS.md` and [skill/references/bot.md](skill/references/bot.md).
- **Changing the implementation:** read [docs/design.md](docs/design.md) and [docs/rpc.md](docs/rpc.md).

The installed Python skill exposes `prime_bot` in the agent's Python environment. It is an async, standard-library-only client for the local RPC socket. Use `help(prime_bot)` for signatures.

### Management checklist

1. Confirm the bridge is reachable with `await prime_bot.status()`. If it is not running, ask the user to start `prime-bot daemon`. Do not start another copy over an existing daemon.
2. Obtain the bot name, persona, and owner Slack user IDs. Names are 1–32 lowercase letters, digits, or hyphens, with no leading or trailing hyphen. Example IDs in this README are placeholders.
3. Call `await prime_bot.create(...)`. Show the returned `manifest` and `secrets_command` to the user. Leave token entry to the terminal workflow above.
4. From a **root** session, call `rlm.create_session` with the returned `session_name` and `dir`. Its bootstrap prompt must tell the bot to read its `AGENTS.md` and the installed bot reference. If you are a sub-agent, ask your root to create the session.
5. Call `await prime_bot.link(name, handle.session_id)`. Use the durable `session_id`, not a display name or `active_session_id`.
6. Check `await prime_bot.status(name)` for missing setup or connection errors. If the bot already exists, inspect it before creating or relinking anything.

The skill contains the complete creation example. Do not use `rlm.spawn` for the bot's root session; reserve it for the bot's own per-thread workers.

### Replying in Slack

Incoming messages include `bot`, `conversation`, `msg`, author information, and addressing hints. Use the supplied conversation key rather than guessing the destination:

```python
await prime_bot.reply("C0123ABC/1730000000.123456", "Investigating the failed build.")
await prime_bot.react("C0123ABC", "1730000012.000200", "eyes")
history = await prime_bot.thread_history("C0123ABC/1730000000.123456")
```

A thread key is `<channel>/<thread_ts>`; an unthreaded DM key is its `D...` channel ID. Bot sessions infer their identity from their working directory. Every incoming message steers the session; keep replies associated with the right conversation. Reply only when confident you are being addressed.

Author and channel names are best-effort, resolved in parallel with a two-second timeout each. Mentions use known bot names or cached names; otherwise they remain Slack IDs without network lookups. Use `user_info()` explicitly when a name matters.

## Trust and permissions

The Unix account is the security boundary. The bridge uses a local Unix socket with mode `0600`, not a TCP listener. Bot and manager sessions under that account are trusted and share RPC authority. They are not isolated from one another, the configuration, or the stored Slack tokens. Keep the account and agent tools limited to authority you intend the bots to have. Slack content can steer those agents.

There are two separate controls:

- **Incoming delivery:** `owners` and `allowed_users` select which people may activate a bot; known prime-bot bots are also accepted. `channels` selects subscribed channels, and `dms` enables incoming DMs. Addressing, thread participation, and a four-bot-message loop guard further limit delivery.
- **Explicit Slack calls:** posts, replies, history reads, and reactions are limited by Slack membership and OAuth scopes—not the incoming delivery settings. The agent must still follow user intent.

History may contain messages from people who cannot activate the bot. Those entries are marked `allowed=false`; bot instructions say not to follow instructions in them. This label is guidance for the agent, not a sandbox against prompt injection.

## Files and configuration

Default home: `~/.prime-bot/`.

| Path | Purpose |
|---|---|
| `config.yaml` | Incoming delivery settings; hand-editable and reloaded live |
| `secrets.yaml` | Slack tokens; written with mode `0600` |
| `state.sqlite` | Durable session links, thread participation, routes, and event deduplication |
| `prime-bot.sock` | Local JSON-RPC socket |
| `bots/<name>/` | Bot git repo and session working directory; contains its persona in `AGENTS.md` |

Configuration list updates replace the whole list; omitted fields stay unchanged. Bots may edit their persona. Treat the home directory as private runtime data, not publishable source.

| Environment variable | Default | Purpose |
|---|---|---|
| `PRIME_BOT_HOME` | `~/.prime-bot` | Runtime home; when overriding, use an absolute path consistently in daemon and clients |
| `PRIME_BOT_PRIME_AGENT` | `prime-agent` | Executable used to deliver messages to sessions |
| `PRIME_BOT_LOG` | `prime_bot=info` | Log filter in `tracing` syntax |
| `PRIME_BOT_SLACK_API_URL` | `https://slack.com/api` | Web API base URL; intended for the fake-Slack tests |

## CLI reference

```text
prime-bot daemon
prime-bot install-skill [--dir <dir>]
prime-bot bot create <name> --owner <U...> [--allowed-user <U...>] [--channel <C...>] [--no-dms] [--persona <text>]
prime-bot bot link <name> <session_id>
prime-bot bot configure <name> [--owner <U...>] [--allowed-user <U...>] [--channel <C...>] [--dms true|false]
prime-bot bot status [<name>]
prime-bot bot manifest <name>
prime-bot secrets set <name>
```

User/channel flags are repeatable. Everything except `daemon` and `install-skill` requires the running bridge. Status and management results are JSON; `bot manifest` prints YAML, and token entry/skill installation print human-readable confirmations.

## Troubleshooting

- **`prime-bot` not found:** add Cargo's bin directory (usually `~/.cargo/bin`) to your shell's `PATH`.
- **Skill missing from the agent:** run `prime-bot install-skill`, then open a fresh root session. Reinstall it after changing the embedded skill.
- **Daemon not running:** start `prime-bot daemon`. If using `PRIME_BOT_HOME`, use the same absolute path in both processes.
- **Bot not connected:** inspect `prime-bot bot status <name>`. `missing` identifies an absent link, app token, or bot token; also check the connection error and daemon logs.
- **Connected but silent:** check the sender's Slack user ID, incoming channel/DM config, and whether the message addresses the bot. The bot may deliberately stay silent in a shared thread.
- **Session replies but Slack stays silent:** confirm it called `prime_bot.reply()` or `post()` rather than only returning a final answer.

## Development

The Rust binary embeds the files in `skill/` at build time. After editing them, rebuild/reinstall the binary and run `prime-bot install-skill` again. Restart the bridge to load a new binary; existing agent kernels may also need restarting to load updated Python code.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
uv run --no-project python -m unittest discover -s tests/python
uv run --no-project --with aiohttp --with pyyaml python tests/e2e/test_e2e.py
```

The end-to-end tests build and run the real daemon with a fake Slack Web API/Socket Mode server and a fake `prime-agent`. They do not need Slack credentials. The Python test commands require [uv](https://docs.astral.sh/uv/).

Current limits: no group DMs, file uploads, or message editing/deletion. Channel delivery requires a mention or existing thread participation. This is an early implementation, not an isolated hosted bot service.

## License

[MIT](LICENSE).
