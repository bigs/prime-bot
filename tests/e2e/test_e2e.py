"""End-to-end test: the real prime-bot daemon against a fake Slack and a fake prime-agent.

Run: uv run --no-project --with aiohttp --with pyyaml python tests/e2e/test_e2e.py
Builds the binary with cargo first.
"""

import asyncio
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skill" / "src"))
sys.path.insert(0, str(Path(__file__).parent))
sys.dont_write_bytecode = True

import prime_bot  # noqa: E402
from fake_slack import FakeSlack  # noqa: E402

BIN = ROOT / "target" / "debug" / "prime-bot"


def msg(user, text, ts, channel="C1", thread_ts=None, channel_type="channel", **extra):
    e = {"type": "message", "channel": channel, "channel_type": channel_type, "user": user, "text": text, "ts": ts}
    if thread_ts:
        e["thread_ts"] = thread_ts
    e.update(extra)
    return e


class E2E(unittest.IsolatedAsyncioTestCase):
    @classmethod
    def setUpClass(cls):
        subprocess.run(["cargo", "build", "--quiet"], cwd=ROOT, check=True)

    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = Path(self.tmp.name) / "home"
        self.agent_log = Path(self.tmp.name) / "agent.jsonl"
        fake_agent = Path(__file__).parent / "fake_prime_agent.py"
        self.slack = FakeSlack()
        await self.slack.start()
        env = dict(os.environ, PRIME_BOT_HOME=str(self.home), PRIME_BOT_PRIME_AGENT=str(fake_agent),
                   PRIME_BOT_SLACK_API_URL=f"http://127.0.0.1:{self.slack.port}/api",
                   FAKE_AGENT_LOG=str(self.agent_log), PRIME_BOT_LOG="prime_bot=debug")
        self.env = env
        os.environ["PRIME_BOT_HOME"] = str(self.home)
        self.daemon_log = open(Path(self.tmp.name) / "daemon.log", "w")
        self.daemon = subprocess.Popen([BIN, "daemon"], env=env, stderr=self.daemon_log)
        await self.until(lambda: (self.home / "prime-bot.sock").exists(), "daemon socket")
        self.events = 0
        self.seen = 0

    async def asyncTearDown(self):
        self.daemon.terminate()
        self.daemon.wait(5)
        self.daemon_log.close()
        if self._outcome.success is False or os.environ.get("SHOW_LOG"):
            print((Path(self.tmp.name) / "daemon.log").read_text()[-6000:])
        await self.slack.stop()
        self.tmp.cleanup()

    async def until(self, cond, what, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if cond():
                return
            await asyncio.sleep(0.05)
        self.fail(f"timed out waiting for {what}")

    def deliveries(self):
        if not self.agent_log.exists():
            return []
        return [json.loads(l) for l in self.agent_log.read_text().splitlines()]

    async def push(self, bot, event, event_id=None):
        self.events += 1
        await self.slack.push(bot, event, event_id or f"Ev{self.events}")

    async def settle(self):
        """Pushes a marker DM to every bot and waits for them, so earlier events have been handled.
        Returns the non-marker deliveries since the last settle."""
        markers = []
        for bot in list(self.slack.sockets):
            self.events += 1
            marker = f"marker {self.events}"
            markers.append(marker)
            await self.slack.push(bot, msg("UOWNER", marker, f"{900 + self.events}.0",
                                           channel="DMARK", channel_type="im"), f"Ev{self.events}")
        await self.until(lambda: all(any(d["text"].endswith(m) for d in self.deliveries()) for m in markers),
                         "marker deliveries")
        new = self.deliveries()[self.seen:]
        self.seen = len(self.deliveries())
        return [d for d in new if "---\nmarker " not in d["text"]]

    def cli(self, *args):
        return subprocess.run([BIN, *args], env=self.env, capture_output=True, text=True)

    async def make_live(self, name, channels):
        r = await prime_bot.create(name, ["UOWNER"], f"{name} persona", allowed_users=["UALLOWED"], channels=channels)
        await prime_bot.link(name, f"session-{name}")
        # secrets_set is CLI-only in the Python client; call the socket directly.
        reader, writer = await asyncio.open_unix_connection(str(self.home / "prime-bot.sock"))
        req = {"jsonrpc": "2.0", "id": 1, "method": "secrets_set",
               "params": {"bot": name, "app_token": f"xapp-{name}", "bot_token": f"xoxb-{name}"}}
        writer.write(json.dumps(req).encode() + b"\n")
        await writer.drain()
        resp = json.loads(await reader.readline())
        writer.close()
        assert "result" in resp, resp
        await self.until(lambda: name in self.slack.sockets, f"{name} socket")
        return r

    async def test_mentions_use_only_known_or_cached_names(self):
        await self.make_live("atlas", ["C1"])
        await self.make_live("nova", ["C1"])
        # Cache an allowed user's name through an ordinary author lookup.
        await self.push("atlas", msg("UALLOWED", "warm author cache", "1.0", channel="D1", channel_type="im"))
        await self.settle()
        calls_before = len(self.slack.calls_to("users.info"))
        unknown = [f"UUNKNOWN{i}" for i in range(40)]
        mentions = ["UATLAS", "UNOVA", "UALLOWED", *unknown]
        body = " ".join(f"<@{uid}>" for uid in mentions)
        await self.push("atlas", msg("UOWNER", body, "2.0"))
        got = await self.settle()
        self.assertEqual(len(got), 1)
        header = got[0]["text"].split("---\n")[0]
        self.assertIn("atlas (UATLAS)", header)
        self.assertIn("nova (UNOVA)", header)
        self.assertIn("name-UALLOWED (UALLOWED)", header)
        expected_mentions = ["atlas (UATLAS)", "nova (UNOVA)", "name-UALLOWED (UALLOWED)", *unknown]
        self.assertIn(f"mentions=[{', '.join(expected_mentions)}]", header)
        # UOWNER was cached by settle; neither cached nor unknown mentions fetch users.
        self.assertEqual(len(self.slack.calls_to("users.info")), calls_before)
        fetched = {p["user"] for _, _, p in self.slack.calls_to("users.info")}
        self.assertTrue(fetched.isdisjoint(unknown))
        self.assertTrue(fetched.isdisjoint({"UATLAS", "UNOVA"}))

    async def test_author_and_channel_lookups_start_in_parallel(self):
        await self.make_live("atlas", ["C1"])
        author_started, author_release = self.slack.gate("users.info", "UOWNER")
        channel_started, channel_release = self.slack.gate("conversations.info", "C1")
        await self.push("atlas", msg("UOWNER", "<@UATLAS> parallel", "1.0"))
        try:
            # Neither response is released: a sequential implementation cannot start both.
            await asyncio.wait_for(asyncio.gather(author_started.wait(), channel_started.wait()), timeout=1.5)
            self.assertEqual(self.deliveries(), [])
        finally:
            author_release.set()
            channel_release.set()
        got = await self.settle()
        self.assertEqual(len(got), 1)
        self.assertIn("from=name-UOWNER (UOWNER, owner)", got[0]["text"])
        self.assertIn("channel=#chan-c1 (C1)", got[0]["text"])

    async def check_lookup_timeout(self, method, target, fallback, successful_lookup):
        await self.make_live("atlas", ["C1", "C2"])
        started, release = self.slack.gate(method, target)
        first = "<@UATLAS> stalled lookup"
        second = "<@UATLAS> next message"
        await self.push("atlas", msg("UOWNER", first, "1.0"))
        await asyncio.wait_for(started.wait(), timeout=1)
        await self.push("atlas", msg("UALLOWED", second, "2.0", channel="C2"))
        self.assertEqual(self.deliveries(), [])
        try:
            # The fake never responds. Both queued messages must still arrive near
            # the two-second enrichment bound, not Slack's much longer HTTP timeout.
            await self.until(lambda: len(self.deliveries()) == 2, "delivery after enrichment timeout", timeout=4)
            got = self.deliveries()
            self.assertEqual([d["text"].split("---\n")[1] for d in got], [first, second])
            self.assertIn(fallback, got[0]["text"])
            self.assertIn(successful_lookup, got[0]["text"])
            self.assertIn("from=name-UALLOWED (UALLOWED, allowed)", got[1]["text"])
            self.assertIn("channel=#chan-c2 (C2)", got[1]["text"])
            self.assertFalse(release.is_set())
        finally:
            release.set()

    async def test_stalled_author_lookup_times_out_without_reordering(self):
        await self.check_lookup_timeout("users.info", "UOWNER", "from=UOWNER (UOWNER, owner)",
                                        "channel=#chan-c1 (C1)")

    async def test_stalled_channel_lookup_times_out_without_reordering(self):
        await self.check_lookup_timeout("conversations.info", "C1", "channel=C1 kind=thread",
                                        "from=name-UOWNER (UOWNER, owner)")

    async def test_everything(self):
        # --- create / link / status ---
        r = await self.make_live("atlas", ["C1"])
        bot_dir = Path(r["dir"])
        self.assertTrue((bot_dir / ".git").is_dir())
        agents = (bot_dir / "AGENTS.md").read_text()
        self.assertIn("atlas persona", agents)
        self.assertIn("references/bot.md", agents)
        self.assertEqual(r["session_name"], "prime-bot-atlas")
        self.assertIn("socket_mode_enabled: true", r["manifest"])
        secrets_mode = (self.home / "secrets.yaml").stat().st_mode & 0o777
        self.assertEqual(secrets_mode, 0o600)
        await self.make_live("nova", ["C1"])

        async def connected():
            s = await prime_bot.status()
            return all(b["connection"] == "connected" for b in s["bots"])
        deadline = time.monotonic() + 10
        while not await connected():
            self.assertLess(time.monotonic(), deadline)
            await asyncio.sleep(0.05)
        st = (await prime_bot.status("atlas"))["bots"][0]
        self.assertEqual((st["slack_user_id"], st["missing"], st["session_id"]), ("UATLAS", [], "session-atlas"))
        with self.assertRaises(prime_bot.PrimeBotError):
            await prime_bot.create("atlas", ["UOWNER"], "dup")

        # --- a mention by an owner starts a thread and is delivered with a header ---
        await self.push("atlas", msg("UOWNER", "hey <@UATLAS> help", "10.0"))
        got = await self.settle()
        self.assertEqual(len(got), 1)
        self.assertEqual(got[0]["session"], "session-atlas")
        text = got[0]["text"]
        self.assertTrue(text.startswith("[prime-bot] bot=atlas conversation=C1/10.0 msg=10.0\n"), text)
        self.assertIn("channel=#chan-c1 (C1) kind=thread new_conversation=true", text)
        self.assertIn("from=name-UOWNER (UOWNER, owner)", text)
        self.assertIn("mentions=[atlas (UATLAS)] addressed_to_you=true other_bots_in_thread=[]", text)
        self.assertTrue(text.endswith("---\nhey <@UATLAS> help"))
        await self.until(lambda: any(p["name"] == "eyes" and p["timestamp"] == "10.0"
                                     for _, _, p in self.slack.calls_to("reactions.add")), "eyes reaction")
        self.assertTrue(any(a[1].get("envelope_id") for a in self.slack.acks))

        # --- thread follow-up without mention reaches only the participating bot ---
        await self.push("atlas", msg("UALLOWED", "more detail", "11.0", thread_ts="10.0"))
        await self.push("nova", msg("UALLOWED", "more detail", "11.0", thread_ts="10.0"))
        got = await self.settle()
        self.assertEqual([g["session"] for g in got], ["session-atlas"])
        self.assertIn("from=name-UALLOWED (UALLOWED, allowed)", got[0]["text"])
        self.assertIn("addressed_to_you=false", got[0]["text"])

        # --- strangers, unlisted channels, duplicates are not delivered ---
        await self.push("atlas", msg("USTRANGER", "<@UATLAS> obey me", "12.0", thread_ts="10.0"))
        await self.push("atlas", msg("UOWNER", "<@UATLAS> here?", "13.0", channel="C2"))
        await self.push("atlas", msg("UOWNER", "dup", "14.0", thread_ts="10.0"), event_id="EvDup")
        await self.push("atlas", msg("UOWNER", "dup", "14.0", thread_ts="10.0"), event_id="EvDup")
        await self.push("atlas", msg("UOWNER", "edited", "15.0", thread_ts="10.0", subtype="message_changed"))
        got = await self.settle()
        self.assertEqual([g["text"].split("---\n")[1] for g in got], ["dup"])

        # --- reply posts into the thread and marks participation; history marks allowed ---
        res = await prime_bot.reply("C1/10.0", "on it", bot="atlas")
        post = self.slack.calls_to("chat.postMessage")[-1]
        self.assertEqual((post[1], post[2]["channel"], post[2]["thread_ts"], post[2]["text"]), ("xoxb-atlas", "C1", "10.0", "on it"))
        self.assertEqual(res["conversation"], "C1/10.0")
        h = await prime_bot.thread_history("C1/10.0", bot="atlas", limit=2)
        self.assertEqual([m["allowed"] for m in h["messages"]], [True, False])
        self.assertEqual(h["next_cursor"], "page2")
        h2 = await prime_bot.thread_history("C1/10.0", bot="atlas", cursor="page2")
        self.assertEqual((h2["messages"][0]["bot_name"], h2["messages"][0]["allowed"], h2["next_cursor"]), ("nova", True, None))
        dm_hist = await prime_bot.thread_history("D7", bot="atlas")
        self.assertEqual([m["text"] for m in dm_hist["messages"]], ["older", "newest"])
        # Explicit calls are governed by Slack access, not incoming subscriptions.
        off_channel = await prime_bot.reply("C9/1.0", "requested reply", bot="atlas")
        self.assertEqual(off_channel["conversation"], "C9/1.0")
        await prime_bot.post("C9", "requested post", bot="atlas")
        await prime_bot.thread_history("C9/1.0", bot="atlas")
        await prime_bot.react("C9", "1.0", "eyes", bot="atlas")
        await self.push("atlas", msg("UOWNER", "<@UATLAS> still not subscribed", "19.0", channel="C9", thread_ts="1.0"))
        self.assertEqual(await self.settle(), [])

        # --- a new top-level post starts a thread the bot participates in ---
        top = await prime_bot.post("C1", "announcement", bot="nova")
        self.assertEqual(top["conversation"], f"C1/{top['ts']}")
        self.assertNotIn("thread_ts", self.slack.calls_to("chat.postMessage")[-1][2])

        # --- a message mentioning only another bot is skipped; both bots see a shared thread ---
        await prime_bot.reply("C1/10.0", "nova here", bot="nova")  # nova now participates too
        await self.push("atlas", msg("UOWNER", "<@UNOVA> you take it", "16.0", thread_ts="10.0"))
        await self.push("nova", msg("UOWNER", "<@UNOVA> you take it", "16.0", thread_ts="10.0"))
        got = await self.settle()
        self.assertEqual([g["session"] for g in got], ["session-nova"])
        self.assertIn("other_bots_in_thread=[atlas]", got[0]["text"])

        # --- loop guard: after 4 bot messages in a row, bot messages stop until a human posts ---
        for i in range(6):
            await self.push("atlas", msg("UNOVA", f"bot says {i}", f"{20 + i}.0", thread_ts="10.0", bot_id="BNOVA"))
        got = await self.settle()
        self.assertEqual([g["text"].split("---\n")[1] for g in got], [f"bot says {i}" for i in range(4)])
        self.assertIn("from=nova (UNOVA, prime-bot)", got[0]["text"])
        await self.push("atlas", msg("UOWNER", "human again", "30.0", thread_ts="10.0"))
        await self.push("atlas", msg("UNOVA", "bot after human", "31.0", thread_ts="10.0", bot_id="BNOVA"))
        got = await self.settle()
        self.assertEqual(len(got), 2)

        # --- bind routes to a sub-agent; a failed binding falls back to root with a note ---
        await prime_bot.bind_thread("C1/10.0", "child-1", bot="atlas")
        threads = (await prime_bot.list_threads(bot="atlas"))["threads"]
        self.assertEqual({t["conversation"]: t["route"] for t in threads}["C1/10.0"], "child-1")
        await self.push("atlas", msg("UOWNER", "to child", "32.0", thread_ts="10.0"))
        got = await self.settle()
        self.assertEqual([g["session"] for g in got], ["child-1"])
        await prime_bot.bind_thread("C1/10.0", "dead-child", bot="atlas")
        await self.push("atlas", msg("UOWNER", "child is gone", "33.0", thread_ts="10.0"))
        got = await self.settle()
        self.assertEqual([g["session"] for g in got], ["session-atlas"])
        self.assertIn("note=the session bound to this conversation (dead-child) failed", got[0]["text"])
        threads = (await prime_bot.list_threads(bot="atlas"))["threads"]
        self.assertIsNone({t["conversation"]: t["route"] for t in threads}["C1/10.0"])
        await prime_bot.bind_thread("C1/10.0", "child-2", bot="atlas")
        await prime_bot.unbind_thread("C1/10.0", bot="atlas")
        self.assertIsNone({t["conversation"]: t["route"] for t in (await prime_bot.list_threads(bot="atlas"))["threads"]}["C1/10.0"])

        # --- DMs: delivered as dm conversations; reply posts without thread_ts; dms=false blocks ---
        await self.push("atlas", msg("UALLOWED", "psst", "40.0", channel="D5", channel_type="im"))
        got = await self.settle()
        self.assertIn("conversation=D5 ", got[0]["text"])
        self.assertIn("kind=dm", got[0]["text"])
        await prime_bot.reply("D5", "hi", bot="atlas")
        self.assertNotIn("thread_ts", self.slack.calls_to("chat.postMessage")[-1][2])
        await prime_bot.configure("atlas", dms=False)
        await prime_bot.reply("D5", "requested DM", bot="atlas")
        await prime_bot.thread_history("D5", bot="atlas")
        await prime_bot.react("D5", "40.0", "eyes", bot="atlas")
        await self.push("atlas", msg("UALLOWED", "psst again", "41.0", channel="D5", channel_type="im"))
        await prime_bot.configure("atlas", dms=True)
        got = await self.settle()
        self.assertEqual(got, [])

        # --- invites: by an owner adds the channel; by anyone else the bot leaves ---
        await self.push("atlas", {"type": "member_joined_channel", "user": "UATLAS", "channel": "C3", "inviter": "UOWNER"})
        await self.push("atlas", {"type": "member_joined_channel", "user": "UATLAS", "channel": "C4", "inviter": "USTRANGER"})
        await self.settle()
        cfg = yaml.safe_load((self.home / "config.yaml").read_text())
        self.assertEqual(cfg["bots"]["atlas"]["channels"], ["C1", "C3"])
        await self.until(lambda: any(p["channel"] == "C4" for _, _, p in self.slack.calls_to("conversations.leave")), "leave C4")
        await self.push("atlas", msg("UOWNER", "<@UATLAS> new channel", "50.0", channel="C3"))
        got = await self.settle()
        self.assertEqual(len(got), 1)

        # --- hand-edited config reloads live ---
        cfg["bots"]["atlas"]["allowed_users"].append("USTRANGER")
        (self.home / "config.yaml").write_text(yaml.safe_dump(cfg))
        await asyncio.sleep(2.5)
        await self.push("atlas", msg("USTRANGER", "now I'm allowed", "60.0", thread_ts="10.0"))
        got = await self.settle()
        self.assertEqual(len(got), 1)

        # --- CLI is an RPC client ---
        out = self.cli("bot", "status", "atlas")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(json.loads(out.stdout)["bots"][0]["connection"], "connected")
        out = self.cli("bot", "configure", "atlas", "--channel", "C1", "--channel", "C3", "--dms", "true")
        self.assertEqual(json.loads(out.stdout), {"owners": ["UOWNER"], "allowed_users": ["UALLOWED", "USTRANGER"],
                                                  "channels": ["C1", "C3"], "dms": True})
        out = self.cli("bot", "manifest", "nova")
        self.assertIn("display_name: nova", out.stdout)
        out = self.cli("bot", "link", "ghost", "x")
        self.assertEqual(out.returncode, 1)
        self.assertIn('unknown bot "ghost"', out.stderr)

        # --- relinking re-points delivery ---
        await prime_bot.link("atlas", "session-atlas-2")
        await self.push("atlas", msg("UOWNER", "after relink", "70.0", thread_ts="10.0"))
        got = await self.settle()
        self.assertEqual([g["session"] for g in got], ["session-atlas-2"])

        # --- a second daemon refuses to start ---
        second = subprocess.run([BIN, "daemon"], env=self.env, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(second.returncode, 0)
        self.assertIn("already running", second.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
