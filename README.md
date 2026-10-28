# prime-bot

Persistent Slack bot personalities backed by [Prime Agent](https://primeintellect.ai) sessions.

- `prime-bot daemon` keeps one Slack Socket Mode connection per bot and delivers each relevant Slack message to the bot's Prime Agent session with `prime-agent send`.
- Bots act in Slack through a local RPC socket, using the `prime_bot` Python client from the bundled Prime Agent skill.
- Bots are created by a Prime Agent session: ask any root session for a Slack bot once the skill is installed.

Design: [docs/design.md](docs/design.md). RPC contract: [docs/rpc.md](docs/rpc.md).

## Install

```sh
cargo install --path .
prime-bot install-skill      # writes ~/.prime/agent/skills/prime-bot
prime-bot daemon             # keep running (e.g. a systemd user service)
```

Then, in a Prime Agent root session: "Create a Slack bot called atlas that ...". The agent creates the bot, shows you a Slack app manifest, and asks you to run `prime-bot secrets set atlas` with the app's tokens.

## CLI

```
prime-bot daemon
prime-bot install-skill [--dir <dir>]
prime-bot bot create <name> --owner <U...> [--allowed-user <U...>] [--channel <C...>] [--no-dms] [--persona <text>]
prime-bot bot link <name> <session_id>
prime-bot bot configure <name> [--owner ...] [--allowed-user ...] [--channel ...] [--dms true|false]
prime-bot bot status [<name>]
prime-bot bot manifest <name>
prime-bot secrets set <name>
```

Everything except `daemon` and `install-skill` talks to the running daemon.

## Files

`~/.prime-bot/` (override with `PRIME_BOT_HOME`): `config.yaml` (incoming message delivery config, hand-editable, reloaded live), `secrets.yaml` (0600), `state.sqlite`, `prime-bot.sock`, and `bots/<name>/` (each bot's git repo and session cwd).

## Trust model

The owning Unix account is the security boundary. Bot and manager sessions under that account are trusted and share RPC authority; they are not isolated from one another or from the account's configuration and secrets. Keep that account and its agent tools limited to authority you intend the bots to have. Slack content can steer those agents.

`channels`, `dms`, and allowed authors control incoming message delivery. Explicit Slack calls, including posts, history, and reactions, are limited by Slack membership and scopes, not those delivery settings. Agents must still follow user intent.

## Environment

| Variable | Default | Purpose |
|---|---|---|
| `PRIME_BOT_HOME` | `~/.prime-bot` | Home directory |
| `PRIME_BOT_PRIME_AGENT` | `prime-agent` | Prime Agent CLI used for delivery |
| `PRIME_BOT_LOG` | `prime_bot=info` | Log filter (`tracing` syntax) |
| `PRIME_BOT_SLACK_API_URL` | `https://slack.com/api` | Slack Web API base URL (tests use a fake Slack) |

## Development

```sh
cargo test                                                                  # unit tests
uv run --no-project python -m unittest discover -s tests/python             # Python client
uv run --no-project --with aiohttp --with pyyaml python tests/e2e/test_e2e.py  # daemon against a fake Slack
```

The skill lives in `skill/` and is embedded into the binary at build time.
