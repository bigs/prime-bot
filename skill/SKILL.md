---
name: prime-bot
description: Create and manage persistent Slack bots backed by Prime Agent sessions. Each bot has its own Slack app, persona, and session. Use when the user wants a Slack bot, a persistent Slack bot personality or assistant, or wants to create, link, relink, configure, or check the status of a prime-bot bot. Also read by bot sessions (see references/bot.md).
---

# prime-bot

prime-bot runs Slack bots. Each bot is a Prime Agent root session that gets Slack messages from the local `prime-bot` daemon and acts in Slack through the `prime_bot` Python module.

This file is for the agent that *manages* bots. If you *are* a bot (your cwd is `~/.prime-bot/bots/<name>`), read [references/bot.md](references/bot.md) instead.

The daemon must be running (`prime-bot daemon`). Calls raise `prime_bot.PrimeBotError` on failure; the message is safe to show the user.

## Create a bot

Only a depth-0 (root) agent can call `rlm.create_session`. If you are a sub-agent, ask your root agent to do this.

1. Get from the user: a bot name (`[a-z0-9-]`, max 32 chars), the persona, the owners' Slack user IDs (`U...`), and optionally other allowed users and channel IDs (`C...`).

2. Create the bot:

   ```python
   result = await prime_bot.create(
       "atlas",
       owners=["U012ABC"],
       persona="Atlas is a terse release engineer who ...",
       allowed_users=["U045DEF"],   # optional
       channels=["C0123ABC"],       # optional
   )
   ```

3. Show the user `result["manifest"]` and `result["secrets_command"]` (`prime-bot secrets set atlas`), and tell them to:
   1. Create a Slack app from the manifest (api.slack.com/apps → Create New App → From a manifest).
   2. Under Basic Information, create an app-level token with the `connections:write` scope.
   3. Install the app to the workspace.
   4. Run `prime-bot secrets set atlas` in a terminal and paste both tokens there.

   Tokens must never be pasted into chat. If the user pastes one, tell them to rotate it.

4. Start the bot session and link it:

   ```python
   h = await rlm.create_session(
       prompt="You are the prime-bot Slack bot atlas. Read AGENTS.md in your working "
              "directory and ~/.prime/agent/skills/prime-bot/references/bot.md, then end your turn. "
              "Slack messages will arrive as [prime-bot] messages.",
       name=result["session_name"],
       cwd=result["dir"],
   )
   await prime_bot.link("atlas", h.session_id)
   ```

The bot goes live once it is linked and has both tokens, in either order.

## Status

```python
await prime_bot.status("atlas")   # or status() for all bots
```

Check `connection` and `missing` (`"link"`, `"app_token"`, `"bot_token"`). The user can also run `prime-bot bot status`.

## Relink to a new session

If the bot's session is lost or the user wants a fresh start, create a new session as in step 4 (same `prompt`, `name`, and `cwd=status["bots"][0]["dir"]`), then call `await prime_bot.link("atlas", h.session_id)`. The old session no longer gets messages. Existing thread bindings stay; the new session sees them in `list_threads` (`route`) and can remove them with `unbind_thread`.

## Configure incoming message delivery

```python
await prime_bot.configure("atlas", allowed_users=["U045DEF", "U078GHI"], dms=True)
```

Only the given fields change; each list replaces the whole list. Changes apply live. Owners can also invite the bot to a channel in Slack; that adds the channel automatically.

`await prime_bot.manifest("atlas")` prints the Slack app manifest again.

## Notes

- Incoming message delivery config lives in `~/.prime-bot/config.yaml`, outside the bot's repo. `channels` and `dms` control what activates the bot, not where explicit Slack calls may act. Slack membership and scopes govern those calls.
- Bot and manager sessions under the same Unix account are trusted, not isolated. File placement and socket permissions do not stop a same-user bot from reading secrets or changing config.
- The bot's persona is in `~/.prime-bot/bots/<name>/AGENTS.md`. The bot may edit it.
- Use `help(prime_bot)` for the full API.
