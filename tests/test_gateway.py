"""Exercise the gateway's auth, streaming and status contracts without Azure."""

import asyncio
import hashlib
import json
import os
import runpy
import sys
import tempfile
import unittest
from collections.abc import Awaitable, Callable
from pathlib import Path
from types import ModuleType
from typing import cast
from unittest.mock import patch

from httpx import ASGITransport, AsyncClient
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.types import ASGIApp, Message, Receive, Scope, Send

import trapi2litellm

KEY = b"fixture-master-key"


async def unused_receive() -> Message:
    raise AssertionError("The fixture must not read a request body")


def scope(key: bytes | None, *, header: bytes = b"authorization") -> Scope:
    value = b"Bearer " + key if key is not None and header == b"authorization" else key
    return {
        "type": "http",
        "http_version": "1.1",
        "method": "POST",
        "path": "/v1/chat/completions",
        "headers": [(header, value)] if value is not None else [],
    }


class GatewayTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.folder: tempfile.TemporaryDirectory[str] = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.root: Path = Path(self.folder.name)
        self.config: Path = self.root / "config.yaml"
        self.config.write_text("model_list: []\n")
        # The environment the proxy last loaded its configuration with; see load().
        self.proxy_environment: dict[str, str] = {}
        self.module: dict[str, object] = self.load({})
        self.gate = cast(Callable[[ASGIApp], ASGIApp], self.module["MasterKeyGate"])

    def load(self, environment: dict[str, str]) -> dict[str, object]:
        """Import the gateway against a stub proxy, without inherited mode settings.

        Records the environment the proxy loads its configuration with in `proxy_environment`.
        """
        proxy = ModuleType("litellm.proxy.proxy_server")

        def proxy_attribute(name: str) -> object:
            if name != "app":
                raise AttributeError(name)
            self.proxy_environment = dict(os.environ)
            return Starlette()

        setattr(proxy, "__getattr__", proxy_attribute)
        with (
            patch.dict(sys.modules, {"litellm.proxy.proxy_server": proxy}),
            patch.dict(
                os.environ,
                {
                    "CONFIG_FILE_PATH": str(self.config),
                    "TRAPI2LITELLM_STATE_DIR": str(self.root),
                    "TRAPI2LITELLM_CONFIG_DIR": str(self.root),
                    "LITELLM_MASTER_KEY": KEY.decode(),
                },
            ),
        ):
            for name in (
                "TRAPI2LITELLM_MODE",
                "TRAPI2LITELLM_UPSTREAM_URL",
                "TRAPI2LITELLM_UPSTREAM_KEY",
            ):
                os.environ.pop(name, None)
            os.environ.update(environment)
            return runpy.run_path(str(Path(trapi2litellm.__file__).parent / "gateway_app.py"))

    def write_upstream_key(self, content: str, mode: int = 0o600) -> None:
        path = self.root / "upstream.env"
        path.write_text(content)
        path.chmod(mode)

    async def status_body(self, module: dict[str, object]) -> dict[str, object]:
        status = cast(Callable[[Request], Awaitable[JSONResponse]], module["status"])
        response = await status(Request(scope(KEY)))
        return json.loads(bytes(response.body))

    async def test_dashboard_shell_is_public_but_all_data_requires_key(self) -> None:
        app = cast(ASGIApp, self.module["app"])
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            for path in ("/gateway", "/gateway/dashboard.js"):
                response = await client.get(path)
                self.assertEqual(response.status_code, 200)
                self.assertNotIn(KEY.decode(), response.text)
                self.assertEqual(response.headers["cache-control"], "no-store")
            for path in ("/gateway/models", "/catalog", "/status"):
                for headers in ({}, {"Authorization": "Bearer wrong"}):
                    self.assertEqual((await client.get(path, headers=headers)).status_code, 401)
            response = await client.get(
                "/gateway/models", headers={"Authorization": f"Bearer {KEY.decode()}"}
            )
            self.assertEqual(response.status_code, 200)
            self.assertEqual(response.json()["data"], [])
            self.assertEqual(response.json()["other_usage"], [])
            self.assertEqual(
                response.headers["x-trapi-config-sha256"],
                hashlib.sha256(self.config.read_bytes()).hexdigest(),
            )
            self.assertEqual(response.headers["cache-control"], "no-store")

    async def test_missing_and_wrong_keys_do_not_reach_upstream(self) -> None:
        async def upstream(scope: Scope, receive: Receive, send: Send) -> None:
            raise AssertionError("An unauthenticated request reached upstream")

        for key in (None, b"wrong-key"):
            with self.subTest(key=key):
                messages: list[Message] = []

                async def send(message: Message) -> None:
                    messages.append(message)

                await self.gate(upstream)(scope(key), unused_receive, send)
                self.assertEqual(messages[0]["status"], 401)

    async def test_stream_frames_arrive_before_upstream_finishes(self) -> None:
        release = asyncio.Event()
        messages: asyncio.Queue[Message] = asyncio.Queue()
        first = b'data: {"delta":"first"}\n\n'
        final = b"data: [DONE]\n\n"

        async def upstream(scope: Scope, receive: Receive, send: Send) -> None:
            await send(
                {
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"text/event-stream")],
                }
            )
            await send({"type": "http.response.body", "body": first, "more_body": True})
            await release.wait()
            await send({"type": "http.response.body", "body": final, "more_body": False})

        async def request() -> None:
            await self.gate(upstream)(scope(KEY, header=b"x-api-key"), unused_receive, messages.put)

        async with asyncio.TaskGroup() as tasks:
            task = tasks.create_task(request())
            start = await asyncio.wait_for(messages.get(), timeout=2)
            body = await asyncio.wait_for(messages.get(), timeout=2)
            self.assertEqual(body["body"], first)
            self.assertTrue(body["more_body"])
            self.assertFalse(task.done())
            digest = hashlib.sha256(self.config.read_bytes()).hexdigest().encode()
            self.assertIn((b"x-trapi-config-sha256", digest), start["headers"])
            release.set()
        body = messages.get_nowait()
        self.assertEqual(body["body"], final)
        self.assertFalse(body["more_body"])

    async def test_status_keeps_deployment_attempt_separate_from_sync(self) -> None:
        (self.root / "sync-status.json").write_text(json.dumps({"status": "ok"}))
        (self.root / "deployment-status.json").write_text(
            json.dumps({"status": "rolled_back", "phase": "activation"})
        )
        body = await self.status_body(self.module)
        self.assertEqual(body["sync-status"], {"status": "ok"})
        self.assertEqual(
            body["deployment-status"], {"status": "rolled_back", "phase": "activation"}
        )

    async def test_status_reports_the_operating_mode(self) -> None:
        body = await self.status_body(self.module)
        self.assertEqual(body["authentication"], "managed_identity")
        self.assertNotIn("upstream_url", body)
        self.assertNotIn("TRAPI2LITELLM_UPSTREAM_KEY", self.proxy_environment)
        upstream_key = "sk-upstream-secret"
        self.write_upstream_key(f"# relay\nTRAPI2LITELLM_UPSTREAM_KEY={upstream_key}\n")
        relay = self.load(
            {
                "TRAPI2LITELLM_MODE": "gateway",
                "TRAPI2LITELLM_UPSTREAM_URL": "http://127.0.0.1:14000",
            }
        )
        body = await self.status_body(relay)
        self.assertEqual(body["authentication"], "gateway")
        self.assertEqual(body["upstream_url"], "http://127.0.0.1:14000")
        self.assertNotIn(upstream_key, json.dumps(body))
        with self.assertRaises(KeyError):
            self.load({"TRAPI2LITELLM_MODE": "gateway"})

    def test_each_worker_reads_the_upstream_key_before_the_proxy_loads(self) -> None:
        relay = {
            "TRAPI2LITELLM_MODE": "gateway",
            "TRAPI2LITELLM_UPSTREAM_URL": "http://127.0.0.1:14000",
        }
        # A reload starts new workers, which pick up a rotated key.
        for key in ("sk-first", "sk-rotated"):
            self.write_upstream_key(f"TRAPI2LITELLM_UPSTREAM_KEY={key}\r\n")
            self.load({**relay, "TRAPI2LITELLM_UPSTREAM_KEY": "sk-inherited"})
            self.assertEqual(self.proxy_environment["TRAPI2LITELLM_UPSTREAM_KEY"], key)
        for content, mode in [
            ("TRAPI2LITELLM_UPSTREAM_KEY=sk-secret\n", 0o644),
            ("TRAPI2LITELLM_UPSTREAM_KEY=sk-secret\n", 0o640),
            ("OTHER=sk-secret\nTRAPI2LITELLM_UPSTREAM_KEY=\n", 0o600),
            ('TRAPI2LITELLM_UPSTREAM_KEY="sk-secret"\n', 0o600),
            ("TRAPI2LITELLM_UPSTREAM_KEY=sk-secret \n", 0o600),
            ("TRAPI2LITELLM_UPSTREAM_KEY=sk-secret\r", 0o600),
        ]:
            self.write_upstream_key(content, mode)
            with self.assertRaises(RuntimeError) as raised:
                self.load(relay)
            self.assertNotIn("sk-secret", str(raised.exception), content)
        (self.root / "upstream.env").unlink()
        os.mkfifo(self.root / "upstream.env", 0o600)
        with self.assertRaisesRegex(RuntimeError, "must be a regular file"):
            self.load(relay)
        (self.root / "upstream.env").unlink()
        with self.assertRaises(FileNotFoundError):
            self.load(relay)


if __name__ == "__main__":
    unittest.main()
