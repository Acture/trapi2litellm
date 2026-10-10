"""Run the real LiteLLM proxy in process and record what it sends to the upstream gateway.

tests/test_relay.py runs this in a subprocess with CONFIG_FILE_PATH set, so the proxy's global
state never reaches other tests. Upstream HTTP is answered in memory; nothing opens a socket.
Arguments: a JSON file of [path, payload] requests and the JSON file to write the results to.
"""

import json
import sys
from pathlib import Path
from unittest.mock import patch

import httpx

CHAT_USAGE: dict[str, object] = {
    "prompt_tokens": 11,
    "completion_tokens": 7,
    "total_tokens": 18,
    "prompt_tokens_details": {"cached_tokens": 2},
}
UPSTREAM: list[dict[str, object]] = []


def answer(request: httpx.Request) -> httpx.Response:
    body: object = json.loads(request.content) if request.content else None
    UPSTREAM.append(
        {
            "method": request.method,
            "url": str(request.url),
            "authorization": request.headers.get("authorization"),
            "body": body,
        }
    )
    model: object = body.get("model") if isinstance(body, dict) else None
    path: str = request.url.path
    if path.endswith("/chat/completions"):
        message = {
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "add", "arguments": '{"a":2,"b":3}'},
                }
            ],
        }
        return httpx.Response(
            200,
            json={
                "id": "chatcmpl-fixture",
                "object": "chat.completion",
                "created": 1,
                "model": model,
                "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls"}],
                "usage": CHAT_USAGE,
            },
        )
    if path.endswith("/responses"):
        text = {"type": "output_text", "text": "OK", "annotations": []}
        return httpx.Response(
            200,
            json={
                "id": "resp_fixture",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": model,
                "output": [
                    {
                        "type": "message",
                        "id": "msg_fixture",
                        "status": "completed",
                        "role": "assistant",
                        "content": [text],
                    }
                ],
                "parallel_tool_calls": True,
                "tool_choice": "auto",
                "tools": [],
                "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7},
            },
        )
    if path.endswith("/embeddings"):
        return httpx.Response(
            200,
            json={
                "object": "list",
                "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
                "model": model,
                "usage": {"prompt_tokens": 3, "total_tokens": 3},
            },
        )
    return httpx.Response(404, json={"error": {"message": "unexpected upstream path"}})


def handle(transport: httpx.HTTPTransport, request: httpx.Request) -> httpx.Response:
    request.read()
    return answer(request)


async def handle_async(
    transport: httpx.AsyncHTTPTransport, request: httpx.Request
) -> httpx.Response:
    await request.aread()
    return answer(request)


def main() -> None:
    requests: list[list[object]] = json.loads(Path(sys.argv[1]).read_text())
    results: dict[str, object] = {}
    with (
        patch.object(httpx.HTTPTransport, "handle_request", handle),
        patch.object(httpx.AsyncHTTPTransport, "handle_async_request", handle_async),
    ):
        from fastapi.testclient import TestClient
        from litellm.proxy.proxy_server import app

        with TestClient(app) as client:
            results["startup"] = list(UPSTREAM)
            client.headers["Authorization"] = "Bearer sk-local"
            exchanges: list[dict[str, object]] = []
            for path, payload in requests:
                UPSTREAM.clear()
                response = client.post(str(path), json=payload)
                exchanges.append(
                    {
                        "status": response.status_code,
                        "response": response.json(),
                        "upstream": list(UPSTREAM),
                    }
                )
            results["exchanges"] = exchanges
    Path(sys.argv[2]).write_text(json.dumps(results))


if __name__ == "__main__":
    main()
