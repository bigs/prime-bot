# prime-bot RPC

JSON-RPC 2.0 over a Unix socket. One JSON object per line (`\n`-terminated), UTF-8.

- Socket: `$PRIME_BOT_HOME/prime-bot.sock`. `PRIME_BOT_HOME` defaults to `~/.prime-bot`.
- A connection may carry any number of requests; responses come back in request order. Clients may also open one connection per call.
- Request: `{"jsonrpc":"2.0","id":1,"method":"reply","params":{...}}`
- Success: `{"jsonrpc":"2.0","id":1,"result":...}`
- Error: `{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"..."}}`

Error codes: `-32700` parse error, `-32601` unknown method, `-32602` invalid params, `-32000` everything else (unknown bot, Slack error, bot not live, ...). `message` is human-readable and safe to show an agent.

## Names

- `bot`: bot name, `[a-z0-9-]`, 1-32 chars, no leading/trailing hyphen.
- `conversation`: `<channel>/<thread_ts>` for a thread, `<channel>` for a DM (outside any thread). Example: `C0123ABC/1730000000.123456`, `D0456DEF`.
- `ts`: Slack message timestamp string, e.g. `1730000012.000200`.

## Methods

Optional params may be omitted or `null`. List params replace the whole list.

### Bot management

**`create`** `{name, owners: [user_id], allowed_users?: [user_id], channels?: [channel_id], dms?: bool = true, persona: string}`
Makes `bots/<name>/`, runs `git init`, writes `AGENTS.md`, adds the config entry and empty secrets. Fails if the bot exists.
Result: `{bot, dir, agents_md_path, manifest: string (YAML), secrets_command: "prime-bot secrets set <name>", session_name: "prime-bot-<name>"}`

**`link`** `{bot, session_id}` — set the bot's root session (durable Prime Agent `sessionId`).
Result: `{bot, session_id}`

**`configure`** `{bot, owners?, allowed_users?, channels?, dms?}` — change incoming message delivery config; applied live.
Result: the bot's config `{owners, allowed_users, channels, dms}`

**`status`** `{bot?}`
Result: `{bots: [BotStatus]}` where `BotStatus` is
`{name, dir, session_id: string|null, tokens: {app_token: bool, bot_token: bool}, connection: "not_live"|"connecting"|"connected"|"error", error: string|null, slack_user_id: string|null, owners, allowed_users, channels, dms, missing: [string]}`.
`missing` lists what keeps the bot from going live: `"link"`, `"app_token"`, `"bot_token"`.

**`manifest`** `{bot}` → `{manifest: string}` (Slack app manifest, YAML)

**`secrets_set`** `{bot, app_token, bot_token}` → `{bot}`. Intended for the hidden-input CLI workflow; same-user socket clients are not assigned separate roles.

### Acting in Slack

Slack API calls require the bot to be live. `channels` and `dms` configure incoming message delivery only; they do not restrict explicit posting, replies, history reads, or reactions. Slack membership and scopes govern access.

**`reply`** `{bot, conversation, text}` — post into a conversation: a thread reply, or a plain DM message for a DM conversation. Marks the bot as participating.
Result: `{channel, ts, conversation}`

**`post`** `{bot, channel, text, thread_ts?}` — post anywhere permitted by Slack membership and scopes. Without `thread_ts`, starts a new top-level message whose thread the bot participates in.
Result: `{channel, ts, conversation}`

**`thread_history`** `{bot, conversation, cursor?, limit?: int = 50}` (limit 1-200) — each page sorted oldest first. For a thread: `conversations.replies`; `next_cursor` moves to newer replies. For a DM: `conversations.history` (top-level DM messages); the first page holds the newest messages and `next_cursor` moves to older ones.
Result: `{messages: [Message], next_cursor: string|null}` where `Message` is
`{ts, thread_ts: string|null, user: string|null, user_name: string|null, is_bot: bool, bot_name: string|null, allowed: bool, text}`.
`bot_name` is set when the author is a prime-bot bot. `allowed` follows the bot's access rules.

**`list_threads`** `{bot, active_since?: float (unix seconds)}` — conversations the bot participates in, most recent first.
Result: `{threads: [{conversation, channel, thread_ts: string|null, kind: "thread"|"dm", last_activity: float, route: string|null}]}`. `route` is the bound session id, or `null` for the root session.

**`react`** `{bot, channel, ts, emoji, remove?: bool = false}` → `{}`. `emoji` without colons.

**`bind_thread`** `{bot, conversation, session_id}` → `{}`
**`unbind_thread`** `{bot, conversation}` → `{}`

**`user_info`** `{bot, user_id}` → `{id, name, real_name, display_name, is_bot}`
