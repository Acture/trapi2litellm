"""LiteLLM on one port, with master-key-only auth and cached discovery status.

The ASGI auth gate also keeps missing/invalid keys out of LiteLLM's database-backed
virtual-key path. Streaming bodies pass through without being buffered.
"""

import hashlib
import hmac
import json
import os
from pathlib import Path

from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.types import ASGIApp, Message, Receive, Scope, Send

CONFIG_PATH: Path = Path(os.environ["CONFIG_FILE_PATH"])
STATE_DIR: Path = Path(os.environ["TRAPI2LITELLM_STATE_DIR"])
CONFIG_SHA256 = hashlib.sha256(CONFIG_PATH.read_bytes()).hexdigest()
MASTER_KEY = os.environ["LITELLM_MASTER_KEY"].encode()
if not MASTER_KEY:
    raise RuntimeError("A nonempty local master key is required")

from litellm.proxy.proxy_server import app as upstream_app


async def catalog(request: Request) -> JSONResponse:
    path = STATE_DIR / "catalog.json"
    if not path.exists():
        return JSONResponse({"error": "No successful catalog sync yet"}, status_code=503)
    return JSONResponse(json.loads(path.read_text()))


async def status(request: Request) -> JSONResponse:
    from importlib.metadata import version

    result = {
        "gateway": "trapi2litellm",
        "litellm_version": version("litellm"),
        "config_sha256": CONFIG_SHA256,
        "authentication": "managed_identity",
    }
    for name in ("sync-status", "sync-error"):
        path = STATE_DIR / (name + ".json")
        if path.exists():
            result[name] = json.loads(path.read_text())
    return JSONResponse(result)


upstream_app.add_route("/catalog", catalog, methods=["GET"])
upstream_app.add_route("/status", status, methods=["GET"])


class MasterKeyGate:
    def __init__(self, wrapped: ASGIApp) -> None:
        self.wrapped = wrapped

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] in ("http", "websocket"):
            headers = dict(scope.get("headers", []))
            auth = headers.get(b"authorization", b"")
            scheme, _, supplied_key = auth.partition(b" ")
            # Also support the standard Anthropic API-key header on this same port.
            if scheme.lower() != b"bearer":
                supplied_key = headers.get(b"x-api-key", b"")
            if not hmac.compare_digest(supplied_key, MASTER_KEY):
                if scope["type"] == "websocket":
                    await send({"type": "websocket.close", "code": 1008})
                else:
                    response = JSONResponse(
                        {
                            "error": {
                                "message": "Invalid or missing gateway API key",
                                "type": "authentication_error",
                            }
                        },
                        status_code=401,
                        headers={"WWW-Authenticate": "Bearer"},
                    )
                    await response(scope, receive, send)
                return

        async def send_with_version(message: Message) -> None:
            if message["type"] == "http.response.start":
                message = dict(message)
                message["headers"] = list(message.get("headers", [])) + [
                    (b"x-trapi-config-sha256", CONFIG_SHA256.encode()),
                ]
            await send(message)

        await self.wrapped(scope, receive, send_with_version)


app = MasterKeyGate(upstream_app)
