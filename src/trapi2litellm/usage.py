"""Private, cross-worker usage observations without rate-limit enforcement."""

from __future__ import annotations

import asyncio
import fcntl
import json
import os
import sqlite3
import time
import uuid
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import cast

from starlette.types import ASGIApp, Message, Receive, Scope, Send

RETENTION_SECONDS = 86_400
WINDOW_SECONDS = 60
LEASE_SECONDS = 30
CAPTURE_BYTES = 1_048_576
INFERENCE_PATHS = frozenset(
    prefix + path
    for prefix in ("", "/v1")
    for path in ("/chat/completions", "/completions", "/responses", "/embeddings", "/messages")
)


def mapping(value: object) -> dict[str, object]:
    if isinstance(value, dict) and all(isinstance(key, str) for key in value):
        return cast(dict[str, object], value)
    return {}


def count(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None


def estimate(value: object) -> int:
    return (len(json.dumps(value, ensure_ascii=False).encode()) + 3) // 4


def text_only(value: object) -> bool:
    """Do not turn image, audio, video or file payload sizes into token guesses."""
    if isinstance(value, list):
        return all(text_only(item) for item in value)
    if isinstance(value, dict):
        fields = mapping(value)
        if fields.keys() & {
            "image_url",
            "image",
            "input_audio",
            "audio",
            "video",
            "file",
            "file_id",
        }:
            return False
        kind = fields.get("type")
        if isinstance(kind, str) and any(
            part in kind for part in ("image", "audio", "video", "file")
        ):
            return False
        return all(text_only(item) for item in fields.values())
    return True


class UsageStore:
    """SQLite transactions aggregate local workers without an external service."""

    def __init__(self, path: Path, clock: Callable[[], float] = time.time) -> None:
        self.path = path
        self.clock = clock
        self.owner = uuid.uuid4().hex
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.path.touch(mode=0o600, exist_ok=True)
        self.path.chmod(0o600)
        # Journal-mode changes need exclusive access and can bypass SQLite's
        # busy timeout. Use a separate inode so macOS SQLite locks do not collide.
        with self.path.with_suffix(".init.lock").open("a+b") as guard:
            os.fchmod(guard.fileno(), 0o600)
            fcntl.flock(guard, fcntl.LOCK_EX)
            with self.connection() as db:
                if db.execute("PRAGMA journal_mode").fetchone()[0] != "wal":
                    db.execute("PRAGMA journal_mode=WAL")
                db.executescript(
                    "CREATE TABLE IF NOT EXISTS workers (id TEXT PRIMARY KEY, updated REAL NOT NULL);"
                    "CREATE TABLE IF NOT EXISTS requests ("
                    "id TEXT PRIMARY KEY, owner TEXT NOT NULL, model TEXT, started REAL NOT NULL,"
                    "finished REAL, status TEXT NOT NULL, input_est INTEGER, output_est INTEGER,"
                    "input_actual INTEGER, output_actual INTEGER);"
                    "CREATE INDEX IF NOT EXISTS requests_started ON requests(started);"
                    "CREATE INDEX IF NOT EXISTS requests_status_owner ON requests(status,owner);"
                )
        self.heartbeat()

    @contextmanager
    def connection(self) -> Iterator[sqlite3.Connection]:
        db = sqlite3.connect(self.path, timeout=5)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def heartbeat(self) -> None:
        now = self.clock()
        with self.connection() as db:
            db.execute("INSERT OR REPLACE INTO workers VALUES (?, ?)", (self.owner, now))
            db.execute(
                "UPDATE requests SET status='abandoned', finished=? "
                "WHERE status='in_flight' AND owner NOT IN "
                "(SELECT id FROM workers WHERE updated>=?)",
                (now, now - LEASE_SECONDS),
            )
            db.execute("DELETE FROM requests WHERE started<?", (now - RETENTION_SECONDS,))
            db.execute("DELETE FROM workers WHERE updated<?", (now - RETENTION_SECONDS,))

    def start(self, request_id: str) -> None:
        with self.connection() as db:
            db.execute(
                "INSERT INTO requests (id,owner,started,status,output_est) VALUES (?,?,?,'in_flight',0)",
                (request_id, self.owner, self.clock()),
            )

    def identify(self, request_id: str, model: str | None, input_est: int | None) -> None:
        with self.connection() as db:
            db.execute(
                "UPDATE requests SET model=?, input_est=? WHERE id=?",
                (model, input_est, request_id),
            )

    def progress(self, request_id: str, output_est: int | None) -> None:
        with self.connection() as db:
            db.execute(
                "UPDATE requests SET output_est=? WHERE id=? AND status='in_flight'",
                (output_est, request_id),
            )

    def finish(
        self,
        request_id: str,
        status: str,
        input_actual: int | None,
        output_actual: int | None,
        output_est: int | None,
    ) -> None:
        with self.connection() as db:
            db.execute(
                "UPDATE requests SET status=?,finished=?,input_actual=?,output_actual=?,output_est=? "
                "WHERE id=? AND status IN ('in_flight','abandoned')",
                (status, self.clock(), input_actual, output_actual, output_est, request_id),
            )

    def snapshot(self) -> dict[str | None, dict[str, object]]:
        now = self.clock()
        # SQL aggregation keeps dashboard memory proportional to model count.
        expressions = ["COUNT(*) AS total", "SUM(started>=:recent) AS last_minute"]
        for status in ("in_flight", "succeeded", "failed", "cancelled", "abandoned"):
            expressions.append(f"SUM(effective_status='{status}') AS {status}")
        expressions.append(
            "SUM((input_actual IS NULL AND input_est IS NULL) OR "
            "(output_actual IS NULL AND output_est IS NULL)) AS unknown_usage_requests"
        )
        for direction in ("input", "output"):
            actual, estimated = direction + "_actual", direction + "_est"
            groups = {
                "reported": ("1", actual),
                "estimated": (f"{actual} IS NULL", estimated),
                "last_minute_reported": ("finished>=:recent", actual),
                "last_minute_estimated": (f"{actual} IS NULL AND finished>=:recent", estimated),
                "in_flight_estimated": (
                    f"{actual} IS NULL AND effective_status='in_flight'",
                    estimated,
                ),
            }
            for group, (condition, value) in groups.items():
                expressions.append(
                    f"COALESCE(SUM(CASE WHEN {condition} THEN {value} ELSE 0 END),0) "
                    f"AS {group}_{direction}"
                )
        with self.connection() as db:
            rows = db.execute(
                "WITH observed AS (SELECT r.*, CASE WHEN status='in_flight' "
                "AND (w.updated IS NULL OR w.updated<:lease) THEN 'abandoned' "
                "ELSE status END AS effective_status FROM requests r "
                "LEFT JOIN workers w ON r.owner=w.id WHERE r.started>=:retained) "
                "SELECT model," + ",".join(expressions) + " FROM observed GROUP BY model",
                {
                    "lease": now - LEASE_SECONDS,
                    "retained": now - RETENTION_SECONDS,
                    "recent": now - WINDOW_SECONDS,
                },
            ).fetchall()
        result: dict[str | None, dict[str, object]] = {}
        for row in rows:
            stats = empty_usage()
            requests = cast(dict[str, int], stats["requests"])
            for key in requests:
                requests[key] = row[key]
            tokens = cast(dict[str, dict[str, int]], stats["tokens"])
            for group, values in tokens.items():
                for direction in values:
                    values[direction] = row[group + "_" + direction]
            stats["unknown_usage_requests"] = row["unknown_usage_requests"]
            result[cast(str | None, row["model"])] = stats
        return result


def empty_usage() -> dict[str, object]:
    return {
        "requests": {
            "total": 0,
            "last_minute": 0,
            "in_flight": 0,
            "succeeded": 0,
            "failed": 0,
            "cancelled": 0,
            "abandoned": 0,
        },
        "tokens": {
            group: {"input": 0, "output": 0}
            for group in (
                "reported",
                "estimated",
                "last_minute_reported",
                "last_minute_estimated",
                "in_flight_estimated",
            )
        },
        "unknown_usage_requests": 0,
    }


@dataclass
class Capture:
    buffer: bytearray = field(default_factory=bytearray)
    overflow: bool = False

    def feed(self, chunk: bytes) -> None:
        if self.overflow:
            return
        if len(self.buffer) + len(chunk) > CAPTURE_BYTES:
            self.buffer.clear()
            self.overflow = True
        else:
            self.buffer.extend(chunk)

    def payload(self) -> dict[str, object]:
        if self.overflow:
            return {}
        try:
            return mapping(json.loads(self.buffer))
        except (json.JSONDecodeError, UnicodeDecodeError):
            # Observation boundary: malformed data still reaches the original app.
            return {}


@dataclass
class ResponseUsage:
    capture: Capture = field(default_factory=Capture)
    sse: bytearray = field(default_factory=bytearray)
    streaming: bool = False
    complete: bool = False
    status: int = 500
    output_bytes: int = 0
    output_unknown: bool = False
    input_actual: int | None = None
    output_actual: int | None = None

    @property
    def output_est(self) -> int | None:
        return None if self.output_unknown else (self.output_bytes + 3) // 4

    def payload(self, payload: dict[str, object]) -> None:
        usage = mapping(payload.get("usage"))
        for nested in ("response", "message"):
            usage = usage or mapping(mapping(payload.get(nested)).get("usage"))
        input_tokens = count(usage.get("prompt_tokens", usage.get("input_tokens")))
        output_tokens = count(usage.get("completion_tokens", usage.get("output_tokens")))
        total_tokens = count(usage.get("total_tokens"))
        if input_tokens is not None:
            self.input_actual = max(self.input_actual or 0, input_tokens)
        if output_tokens is not None:
            self.output_actual = max(self.output_actual or 0, output_tokens)
        elif input_tokens is not None and total_tokens is not None and total_tokens >= input_tokens:
            self.output_actual = total_tokens - input_tokens
        if not self.streaming:
            if self.status < 400:
                selected = {
                    key: payload[key] for key in ("choices", "output", "content") if key in payload
                }
                if selected and text_only(selected):
                    self.output_bytes = len(json.dumps(selected, ensure_ascii=False).encode())
                elif self.output_actual is None:
                    self.output_unknown = True
            return
        if not text_only(payload):
            self.output_unknown = True
        delta = payload.get("delta")
        if isinstance(delta, str):
            self.output_bytes += len(delta.encode())
        elif isinstance(delta, dict):
            self.output_bytes += sum(
                len(value.encode())
                for value in delta.values()
                if isinstance(value, str) and value != delta.get("type")
            )
        choices = payload.get("choices")
        if isinstance(choices, list):
            for choice in choices:
                chunk = mapping(mapping(choice).get("delta"))
                for key in ("content", "refusal", "reasoning_content", "reasoning"):
                    value = chunk.get(key)
                    if isinstance(value, str):
                        self.output_bytes += len(value.encode())
                    elif value is not None:
                        self.output_unknown = True
                text = mapping(choice).get("text")
                if isinstance(text, str):
                    self.output_bytes += len(text.encode())
                tools = chunk.get("tool_calls")
                if isinstance(tools, list):
                    for tool in tools:
                        function = mapping(mapping(tool).get("function"))
                        self.output_bytes += sum(
                            len(value.encode())
                            for value in function.values()
                            if isinstance(value, str)
                        )

    def feed(self, message: Message) -> None:
        if message["type"] == "http.response.start":
            self.status = message["status"]
            self.streaming = b"text/event-stream" in dict(message.get("headers", [])).get(
                b"content-type", b""
            )
            return
        if message["type"] != "http.response.body":
            return
        chunk = message.get("body", b"")
        if self.streaming:
            for offset in range(0, len(chunk), 65_536):
                self.sse.extend(chunk[offset : offset + 65_536])
                self.sse[:] = self.sse.replace(b"\r\n", b"\n")
                while b"\n\n" in self.sse:
                    frame, _, rest = self.sse.partition(b"\n\n")
                    self.sse[:] = rest
                    if len(frame) > CAPTURE_BYTES:
                        self.output_unknown = True
                        continue
                    data = b"\n".join(
                        line[5:].lstrip()
                        for line in frame.splitlines()
                        if line.startswith(b"data:")
                    )
                    if data and data != b"[DONE]":
                        try:
                            self.payload(mapping(json.loads(data)))
                        except (json.JSONDecodeError, UnicodeDecodeError):
                            self.output_unknown = True
                if len(self.sse) > CAPTURE_BYTES:
                    self.sse.clear()
                    self.output_unknown = True
        else:
            self.capture.feed(chunk)
        if not message.get("more_body", False):
            self.complete = True
            if not self.streaming:
                self.payload(self.capture.payload())
                self.output_unknown |= self.capture.overflow


class UsageRecorder:
    def __init__(self, wrapped: ASGIApp, store: UsageStore, model_names: frozenset[str]) -> None:
        self.wrapped = wrapped
        self.store = store
        self.model_names = model_names
        self.slots = asyncio.Semaphore(2)

    async def run[**P, T](self, operation: Callable[P, T], *args: P.args, **kwargs: P.kwargs) -> T:
        async with self.slots:
            return await asyncio.to_thread(operation, *args, **kwargs)

    async def heartbeat(self) -> None:
        while True:
            await self.run(self.store.heartbeat)
            await asyncio.sleep(5)

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] == "lifespan":
            async with asyncio.TaskGroup() as tasks:
                heartbeat = tasks.create_task(self.heartbeat())
                try:
                    await self.wrapped(scope, receive, send)
                finally:
                    heartbeat.cancel()
            return
        if (
            scope["type"] != "http"
            or scope.get("method") != "POST"
            or scope.get("path") not in INFERENCE_PATHS
        ):
            await self.wrapped(scope, receive, send)
            return
        request_id = uuid.uuid4().hex
        await self.run(self.store.start, request_id)
        request = Capture()
        response = ResponseUsage()
        last_progress = 0.0
        last_output: int | None = 0

        async def observed_receive() -> Message:
            message = await receive()
            if message["type"] == "http.request":
                request.feed(message.get("body", b""))
                if not message.get("more_body", False):
                    payload = request.payload()
                    model = payload.get("model")
                    selected = {
                        key: payload[key]
                        for key in ("messages", "input", "prompt", "instructions", "tools")
                        if key in payload
                    }
                    await self.run(
                        self.store.identify,
                        request_id,
                        model if isinstance(model, str) and model in self.model_names else None,
                        estimate(selected) if selected and text_only(selected) else None,
                    )
            return message

        async def observed_send(message: Message) -> None:
            nonlocal last_progress, last_output
            response.feed(message)
            await send(message)
            if (
                response.streaming
                and response.output_est != last_output
                and time.monotonic() - last_progress >= 1
            ):
                await self.run(self.store.progress, request_id, response.output_est)
                last_output = response.output_est
                last_progress = time.monotonic()

        outcome = "failed"
        try:
            await self.wrapped(scope, observed_receive, observed_send)
            if response.complete and response.status < 400:
                outcome = "succeeded"
        except asyncio.CancelledError:
            outcome = "cancelled"
            raise
        finally:
            await self.run(
                self.store.finish,
                request_id,
                outcome,
                response.input_actual,
                response.output_actual,
                response.output_est,
            )
