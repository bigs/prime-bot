import asyncio
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True  # skill/ is embedded verbatim; keep __pycache__ out of it
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "skill" / "src"))

import prime_bot  # noqa: E402


class FakeDaemon:
    """JSON-RPC server on a Unix socket that records requests."""

    def __init__(self, path: Path):
        self.path = path
        self.requests = []
        self.responses = {}  # method -> {"result": ...} or {"error": ...}

    async def start(self):
        self.server = await asyncio.start_unix_server(self._handle, path=str(self.path))

    async def stop(self):
        self.server.close()
        await self.server.wait_closed()

    async def _handle(self, reader, writer):
        while line := await reader.readline():
            request = json.loads(line)
            self.requests.append(request)
            body = self.responses.get(request["method"], {"result": {}})
            response = {"jsonrpc": "2.0", "id": request["id"], **body}
            writer.write(json.dumps(response).encode() + b"\n")
            await writer.drain()
        writer.close()


class ClientTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.home = Path(tempfile.mkdtemp(prefix="pb-"))
        self.old_env = os.environ.get("PRIME_BOT_HOME")
        self.old_cwd = os.getcwd()
        os.environ["PRIME_BOT_HOME"] = str(self.home)
        (self.home / "bots" / "atlas" / "sub").mkdir(parents=True)
        self.daemon = FakeDaemon(self.home / "prime-bot.sock")
        await self.daemon.start()

    async def asyncTearDown(self):
        await self.daemon.stop()
        os.chdir(self.old_cwd)
        if self.old_env is None:
            os.environ.pop("PRIME_BOT_HOME", None)
        else:
            os.environ["PRIME_BOT_HOME"] = self.old_env
        shutil.rmtree(self.home)

    @property
    def last(self):
        return self.daemon.requests[-1]

    async def test_request_shape_omits_none(self):
        self.daemon.responses["post"] = {"result": {"channel": "C1", "ts": "1.2", "conversation": "C1/1.2"}}
        result = await prime_bot.post("C1", "hi", bot="atlas")
        self.assertEqual(result, {"channel": "C1", "ts": "1.2", "conversation": "C1/1.2"})
        self.assertEqual(self.last["jsonrpc"], "2.0")
        self.assertIsInstance(self.last["id"], int)
        self.assertEqual(self.last["method"], "post")
        self.assertEqual(self.last["params"], {"bot": "atlas", "channel": "C1", "text": "hi"})

    async def test_create_params(self):
        await prime_bot.create("atlas", ["U1"], "Helpful.", channels=["C1"], dms=False)
        self.assertEqual(self.last["method"], "create")
        self.assertEqual(
            self.last["params"],
            {"name": "atlas", "owners": ["U1"], "persona": "Helpful.", "channels": ["C1"], "dms": False},
        )

    async def test_manifest_returns_string(self):
        self.daemon.responses["manifest"] = {"result": {"manifest": "display_information: {}"}}
        self.assertEqual(await prime_bot.manifest("atlas"), "display_information: {}")
        self.assertEqual(self.last["params"], {"bot": "atlas"})

    async def test_react_strips_colons(self):
        await prime_bot.react("C1", "1.2", ":eyes:", bot="atlas")
        self.assertEqual(
            self.last["params"],
            {"bot": "atlas", "channel": "C1", "ts": "1.2", "emoji": "eyes", "remove": False},
        )

    async def test_bot_inferred_from_cwd(self):
        os.chdir(self.home / "bots" / "atlas" / "sub")
        await prime_bot.reply("C1/1.2", "hello")
        self.assertEqual(self.last["params"], {"bot": "atlas", "conversation": "C1/1.2", "text": "hello"})

    async def test_bot_inferred_through_symlink(self):
        link = self.home / "link"
        link.symlink_to(self.home / "bots" / "atlas")
        os.chdir(link)
        await prime_bot.list_threads()
        self.assertEqual(self.last["params"], {"bot": "atlas"})

    async def test_explicit_bot_overrides_cwd(self):
        os.chdir(self.home / "bots" / "atlas")
        await prime_bot.unbind_thread("C1/1.2", bot="nova")
        self.assertEqual(self.last["params"]["bot"], "nova")

    async def test_bot_not_inferable(self):
        os.chdir(self.home)
        with self.assertRaises(prime_bot.PrimeBotError) as ctx:
            await prime_bot.reply("C1/1.2", "hello")
        self.assertIsNone(ctx.exception.code)
        self.assertIn("cannot infer the bot", str(ctx.exception))
        self.assertEqual(self.daemon.requests, [])

    async def test_status_outside_bot_dir_lists_all(self):
        os.chdir(self.home)
        await prime_bot.status()
        self.assertEqual(self.last["params"], {})
        os.chdir(self.home / "bots" / "atlas")
        await prime_bot.status()
        self.assertEqual(self.last["params"], {"bot": "atlas"})

    async def test_error_mapping(self):
        self.daemon.responses["reply"] = {"error": {"code": -32000, "message": "bot atlas is not live"}}
        with self.assertRaises(prime_bot.PrimeBotError) as ctx:
            await prime_bot.reply("C1/1.2", "hello", bot="atlas")
        self.assertEqual(ctx.exception.code, -32000)
        self.assertEqual(ctx.exception.message, "bot atlas is not live")

    async def test_large_response(self):
        text = "x" * 200_000
        self.daemon.responses["thread_history"] = {"result": {"messages": [{"text": text}], "next_cursor": None}}
        result = await prime_bot.thread_history("C1/1.2", bot="atlas")
        self.assertEqual(result["messages"][0]["text"], text)

    async def test_missing_socket(self):
        await self.daemon.stop()
        (self.home / "prime-bot.sock").unlink(missing_ok=True)
        with self.assertRaises(prime_bot.PrimeBotError) as ctx:
            await prime_bot.status()
        self.assertIn("prime-bot daemon is not running", str(ctx.exception))
        self.assertIn("prime-bot daemon", ctx.exception.message)
        await self.daemon.start()


if __name__ == "__main__":
    unittest.main()
