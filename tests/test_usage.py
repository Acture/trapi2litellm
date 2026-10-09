"""Offline model metadata and cross-worker usage contracts, with no inference."""

import asyncio
import hashlib
import json
import multiprocessing
import tempfile
import unittest
from pathlib import Path
from typing import cast

from starlette.types import ASGIApp, Message, Receive, Scope, Send

from trapi2litellm.model_view import ModelView, rate_limits
from trapi2litellm.usage import CAPTURE_BYTES, ResponseUsage, UsageRecorder, UsageStore


def write_worker(path: str, prefix: str) -> None:
    store = UsageStore(Path(path))
    for index in range(12):
        request = f"{prefix}-{index}"
        store.start(request)
        store.identify(request, "trapi/test", 999)
        store.finish(request, "succeeded", 10, 5, 999)


def requests(stats: dict[str, object]) -> dict[str, int]:
    return cast(dict[str, int], stats["requests"])


def tokens(stats: dict[str, object]) -> dict[str, dict[str, int]]:
    return cast(dict[str, dict[str, int]], stats["tokens"])


class StoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.folder: tempfile.TemporaryDirectory[str] = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.path = Path(self.folder.name) / "usage.sqlite3"
        self.now = 100_000.0
        self.store = UsageStore(self.path, lambda: self.now)

    def test_reported_usage_replaces_estimates_and_finish_is_idempotent(self) -> None:
        self.store.start("one")
        self.store.identify("one", "model", 900)
        self.store.finish("one", "succeeded", 10, 20, 800)
        self.store.finish("one", "failed", 100, 200, 800)
        stats = self.store.snapshot()["model"]
        self.assertEqual(requests(stats)["total"], 1)
        self.assertEqual(requests(stats)["succeeded"], 1)
        self.assertEqual(tokens(stats)["reported"], {"input": 10, "output": 20})
        self.assertEqual(tokens(stats)["estimated"], {"input": 0, "output": 0})
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)

    def test_window_uses_arrival_for_requests_and_completion_for_tokens(self) -> None:
        self.store.start("long")
        self.store.identify("long", "model", 15)
        self.now += 70
        self.store.heartbeat()
        self.assertEqual(requests(self.store.snapshot()["model"])["last_minute"], 0)
        self.assertEqual(tokens(self.store.snapshot()["model"])["in_flight_estimated"]["input"], 15)
        self.store.finish("long", "succeeded", None, None, 8)
        stats = self.store.snapshot()["model"]
        self.assertEqual(tokens(stats)["last_minute_estimated"], {"input": 15, "output": 8})
        self.now += 61
        self.assertEqual(
            tokens(self.store.snapshot()["model"])["last_minute_estimated"],
            {"input": 0, "output": 0},
        )
        self.now += 86_400
        self.store.heartbeat()
        self.assertEqual(self.store.snapshot(), {})
        with self.store.connection() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM requests").fetchone()[0], 0)

    def test_reload_retains_counts_and_stale_workers_stop_counting_as_active(self) -> None:
        self.store.start("old-worker")
        self.store.identify("old-worker", "model", None)
        reloaded = UsageStore(self.path, lambda: self.now)
        self.assertEqual(requests(reloaded.snapshot()["model"])["in_flight"], 1)
        self.now += 31
        stats = reloaded.snapshot()["model"]
        self.assertEqual(requests(stats)["in_flight"], 0)
        self.assertEqual(requests(stats)["abandoned"], 1)
        self.assertEqual(stats["unknown_usage_requests"], 1)
        reloaded.heartbeat()
        self.assertEqual(requests(reloaded.snapshot()["model"])["abandoned"], 1)

    def test_two_processes_share_counts_without_lost_updates(self) -> None:
        context = multiprocessing.get_context("spawn")
        processes = [
            context.Process(target=write_worker, args=(str(self.path), str(index)))
            for index in range(2)
        ]
        for process in processes:
            process.start()
        try:
            for process in processes:
                process.join(timeout=15)
                self.assertEqual(process.exitcode, 0)
        finally:
            for process in processes:
                if process.is_alive():
                    process.terminate()
                    process.join(timeout=5)
                process.close()
        # Child clocks use wall time; the reloaded observer uses the same clock.
        stats = UsageStore(self.path).snapshot()["trapi/test"]
        self.assertEqual(requests(stats)["total"], 24)
        self.assertEqual(tokens(stats)["reported"], {"input": 240, "output": 120})


class ModelTests(unittest.TestCase):
    def test_limit_shapes_and_unknown_are_distinct(self) -> None:
        limits = rate_limits(
            {
                "requests": {"count": 12},
                "tokens": 400,
                "rpm": 0,
                "tpm": -1,
                "other": True,
                "bad": -2,
            }
        )
        self.assertEqual(
            [item["state"] for item in limits],
            ["limited", "limited", "unlimited", "unlimited", "unknown", "unknown"],
        )
        self.assertEqual(limits[0]["per_minute"], 12)
        self.assertEqual(limits[1]["unit"], "tok/min")
        self.assertEqual(rate_limits(None), [])

    def test_metadata_comes_from_running_snapshot_and_sync_hash_must_match(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            config = json.dumps(
                {
                    "model_list": [
                        {
                            "model_name": "model",
                            "model_info": {
                                "capabilities": {"chat": True},
                                "rate_limits": {"requests": 12},
                            },
                        }
                    ]
                }
            ).encode()
            store = UsageStore(root / "usage.sqlite3")
            store.start("historic")
            store.identify("historic", "removed-model", 4)
            store.finish("historic", "failed", None, None, 0)
            view = ModelView(config, root, store)
            (root / "catalog.json").write_text(
                '{"fetched_at":"new-catalog","RateLimits":{"requests":99}}'
            )
            (root / "sync-status.json").write_text(
                '{"config_sha256":"different","checked_at":"new-sync"}'
            )
            result = view.snapshot()
            models = cast(list[dict[str, object]], result["data"])
            self.assertEqual(models[0]["upstream_rate_limits"], {"requests": 12})
            self.assertIsNone(result["running_config_synced_at"])
            self.assertEqual(result["catalog_fetched_at"], "new-catalog")
            self.assertEqual(len(cast(list[object], result["other_usage"])), 1)
            (root / "sync-status.json").write_text(
                json.dumps(
                    {
                        "config_sha256": hashlib.sha256(config).hexdigest(),
                        "checked_at": "matching-sync",
                    }
                )
            )
            self.assertEqual(view.snapshot()["running_config_synced_at"], "matching-sync")


class RecorderTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.folder: tempfile.TemporaryDirectory[str] = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.path = Path(self.folder.name) / "usage.sqlite3"
        self.store = UsageStore(self.path)

    async def invoke(
        self,
        app: ASGIApp,
        payload: dict[str, object],
        *,
        path: str = "/v1/chat/completions",
        send: Send | None = None,
    ) -> list[Message]:
        body = json.dumps(payload, ensure_ascii=False).encode()
        messages: list[Message] = []
        consumed = False

        async def receive() -> Message:
            nonlocal consumed
            if consumed:
                raise AssertionError("Observer must not read the body twice")
            consumed = True
            return {"type": "http.request", "body": body, "more_body": False}

        async def collect(message: Message) -> None:
            messages.append(message)

        scope: Scope = {"type": "http", "method": "POST", "path": path}
        await UsageRecorder(app, self.store, frozenset({"model", "text", "vision"}))(
            scope, receive, send or collect
        )
        return messages

    def responder(self, result: dict[str, object], status: int = 200) -> ASGIApp:
        async def app(scope: Scope, receive: Receive, send: Send) -> None:
            await receive()
            await send({"type": "http.response.start", "status": status, "headers": []})
            await send({"type": "http.response.body", "body": json.dumps(result).encode()})

        return app

    async def test_actual_response_usage_and_privacy(self) -> None:
        secret = "private-prompt-and-reply"
        await self.invoke(
            self.responder(
                {
                    "choices": [{"message": {"content": secret}}],
                    "usage": {"prompt_tokens": 7, "completion_tokens": 3},
                }
            ),
            {"model": "model", "messages": [{"content": secret}]},
        )
        stats = self.store.snapshot()["model"]
        self.assertEqual(tokens(stats)["reported"], {"input": 7, "output": 3})
        self.assertEqual(tokens(stats)["estimated"], {"input": 0, "output": 0})
        with self.store.connection() as db:
            self.assertNotIn(secret, " ".join(db.iterdump()))
        for path in self.path.parent.iterdir():
            self.assertNotIn(secret.encode(), path.read_bytes())

    async def test_missing_usage_is_estimated_and_multimodal_is_unknown(self) -> None:
        app = self.responder({"choices": [{"message": {"content": "hello"}}]})
        await self.invoke(app, {"model": "text", "messages": [{"content": "hello"}]})
        stats = self.store.snapshot()["text"]
        self.assertGreater(tokens(stats)["estimated"]["input"], 0)
        self.assertGreater(tokens(stats)["estimated"]["output"], 0)
        self.assertEqual(stats["unknown_usage_requests"], 0)
        await self.invoke(
            app,
            {
                "model": "vision",
                "messages": [
                    {
                        "content": [
                            {
                                "type": "image_url",
                                "image_url": {"url": "data:image/png;base64,AAAA"},
                            }
                        ]
                    }
                ],
            },
        )
        self.assertEqual(self.store.snapshot()["vision"]["unknown_usage_requests"], 1)

    async def test_failed_cancelled_and_raised_requests_are_observed(self) -> None:
        payload: dict[str, object] = {"model": "model", "input": "hello"}
        await self.invoke(self.responder({"error": "fixture"}, 429), payload)

        async def cancelled(scope: Scope, receive: Receive, send: Send) -> None:
            await receive()
            raise asyncio.CancelledError

        async def raised(scope: Scope, receive: Receive, send: Send) -> None:
            await receive()
            raise ValueError("fixture failure")

        with self.assertRaises(asyncio.CancelledError):
            await self.invoke(cancelled, payload)
        with self.assertRaisesRegex(ValueError, "fixture failure"):
            await self.invoke(raised, payload)
        stats = self.store.snapshot()["model"]
        self.assertEqual(requests(stats)["failed"], 2)
        self.assertEqual(requests(stats)["cancelled"], 1)
        self.assertEqual(requests(stats)["in_flight"], 0)

    async def test_stream_is_passthrough_and_final_usage_replaces_live_estimate(self) -> None:
        release = asyncio.Event()
        ready = asyncio.Event()
        messages: asyncio.Queue[Message] = asyncio.Queue()
        first = 'data: {"choices":[{"delta":{"content":"你好"}}]}\r\n\r\n'.encode()
        last = b'data: {"usage":{"prompt_tokens":11,"completion_tokens":2}}\n\ndata: [DONE]\n\n'

        async def stream(scope: Scope, receive: Receive, send: Send) -> None:
            await receive()
            await send(
                {
                    "type": "http.response.start",
                    "status": 200,
                    "headers": [(b"content-type", b"text/event-stream")],
                }
            )
            for chunk in (first[:30], first[30:]):
                await send({"type": "http.response.body", "body": chunk, "more_body": True})
            ready.set()
            await release.wait()
            await send({"type": "http.response.body", "body": last, "more_body": False})

        async with asyncio.TaskGroup() as tasks:
            task = tasks.create_task(
                self.invoke(stream, {"model": "model", "input": "hello"}, send=messages.put)
            )
            await asyncio.wait_for(messages.get(), 2)
            for chunk in (first[:30], first[30:]):
                self.assertEqual((await asyncio.wait_for(messages.get(), 2))["body"], chunk)
            self.assertFalse(task.done())
            await asyncio.wait_for(ready.wait(), 2)
            stats = await asyncio.to_thread(self.store.snapshot)
            self.assertEqual(requests(stats["model"])["in_flight"], 1)
            self.assertEqual(tokens(stats["model"])["in_flight_estimated"]["output"], 2)
            release.set()
        self.assertEqual(messages.get_nowait()["body"], last)
        self.assertEqual(
            tokens(self.store.snapshot()["model"])["reported"], {"input": 11, "output": 2}
        )
        self.assertEqual(
            tokens(self.store.snapshot()["model"])["estimated"], {"input": 0, "output": 0}
        )

    async def test_oversized_request_is_unknown_and_stays_unchanged(self) -> None:
        size = CAPTURE_BYTES + 10
        seen = 0

        async def app(scope: Scope, receive: Receive, send: Send) -> None:
            nonlocal seen
            seen = len(json.loads((await receive())["body"])["input"])
            await send({"type": "http.response.start", "status": 400, "headers": []})
            await send({"type": "http.response.body", "body": b"{}"})

        await self.invoke(app, {"model": "model", "input": "x" * size})
        self.assertEqual(seen, size)
        self.assertEqual(self.store.snapshot()[None]["unknown_usage_requests"], 1)

    def test_fragmented_sse_usage_variants_and_bounded_capture(self) -> None:
        parser = ResponseUsage()
        parser.feed(
            {
                "type": "http.response.start",
                "status": 200,
                "headers": [(b"content-type", b"text/event-stream")],
            }
        )
        data = 'data: {"delta":"你好"}\r\n\r\ndata: {"response":{"usage":{"input_tokens":4,"output_tokens":2}}}\n\n'.encode()
        for byte in data:
            parser.feed({"type": "http.response.body", "body": bytes([byte]), "more_body": True})
        self.assertEqual(parser.output_est, 2)
        self.assertEqual((parser.input_actual, parser.output_actual), (4, 2))
        parser.feed(
            {
                "type": "http.response.body",
                "body": b"data: " + b"x" * (CAPTURE_BYTES * 2),
                "more_body": True,
            }
        )
        self.assertLessEqual(len(parser.sse), CAPTURE_BYTES)
        self.assertIsNone(parser.output_est)

    async def test_non_inference_routes_do_not_count_as_requests(self) -> None:
        await self.invoke(self.responder({}), {}, path="/gateway/models")
        self.assertEqual(self.store.snapshot(), {})

    async def test_unconfigured_model_is_grouped_without_persisting_client_string(self) -> None:
        await self.invoke(
            self.responder({}, 400), {"model": "arbitrary-client-string", "input": "hello"}
        )
        self.assertEqual(set(self.store.snapshot()), {None})
        with self.store.connection() as db:
            self.assertNotIn("arbitrary-client-string", " ".join(db.iterdump()))


if __name__ == "__main__":
    unittest.main()
