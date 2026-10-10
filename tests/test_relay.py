"""Forward requests through the real LiteLLM proxy with the gateway relay configuration.

The configuration is the snapshot the native renderer must reproduce (catalog.rs). The proxy runs
in a subprocess with in-memory upstream HTTP, so the check needs neither sockets nor credentials.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import cast

TESTS: Path = Path(__file__).resolve().parent
UPSTREAM_KEY = "sk-upstream-fixture"
TOOLS: list[dict[str, object]] = [
    {
        "type": "function",
        "function": {
            "name": "add",
            "parameters": {"type": "object", "properties": {"a": {"type": "integer"}}},
        },
    }
]
HELLO: list[dict[str, str]] = [{"role": "user", "content": "hi"}]
# Each request pins a parameter or route that LiteLLM's `openai` provider dropped, rejected or
# rerouted for a `trapi/` model while the managed-identity `azure` provider forwarded it.
REQUESTS: list[tuple[str, dict[str, object]]] = [
    (
        "/v1/chat/completions",
        {
            "model": "trapi/gpt-5.2_2025-12-11",
            "messages": HELLO,
            "tools": TOOLS,
            "tool_choice": "required",
            "top_p": 0.9,
            "logprobs": True,
            "reasoning_effort": "none",
            "user": "u1",
        },
    ),
    (
        "/v1/chat/completions",
        {
            "model": "trapi/gpt-5.2_2025-12-11",
            "messages": HELLO,
            "temperature": 0.2,
            "reasoning_effort": "xhigh",
        },
    ),
    (
        "/v1/chat/completions",
        {"model": "trapi/o3-mini_2025-01-31", "messages": HELLO, "reasoning_effort": "high"},
    ),
    (
        "/v1/chat/completions",
        {"model": "trapi/gpt-5-codex_2025-09-15", "messages": HELLO, "max_completion_tokens": 64},
    ),
    (
        "/v1/responses",
        {"model": "trapi/gpt-5.2_2025-12-11", "input": "hi", "reasoning": {"effort": "none"}},
    ),
    ("/v1/embeddings", {"model": "trapi/text-embedding-3-large_1", "input": ["hi"]}),
    (
        "/v1/messages",
        {"model": "trapi/gpt-4o-mini_2024-07-18", "max_tokens": 64, "messages": HELLO},
    ),
    ("/v1/messages", {"model": "trapi/Qwen/Qwen3.5-9B", "max_tokens": 64, "messages": HELLO}),
]


def relay(requests: list[tuple[str, dict[str, object]]]) -> dict[str, object]:
    """What the proxy answers and sends upstream for each request."""
    with tempfile.TemporaryDirectory() as folder:
        root = Path(folder)
        config = json.loads((TESTS / "snapshots/gateway/config.json").read_text())
        # The in-memory upstream intercepts httpx; the default aiohttp transport sits below the
        # parameter mapping under test.
        config["litellm_settings"]["disable_aiohttp_transport"] = True
        (root / "config.yaml").write_text(json.dumps(config))
        (root / "requests.json").write_text(json.dumps(requests))
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.lower().endswith("_proxy") and key != "TRAPI2LITELLM_UPSTREAM_KEY"
        }
        subprocess.run(
            [
                sys.executable,
                "-I",
                str(TESTS / "relay_proxy.py"),
                str(root / "requests.json"),
                str(root / "results.json"),
            ],
            env={
                **environment,
                "CONFIG_FILE_PATH": str(root / "config.yaml"),
                "LITELLM_MASTER_KEY": "sk-local",
                "TRAPI2LITELLM_UPSTREAM_KEY": UPSTREAM_KEY,
                "LITELLM_LOCAL_MODEL_COST_MAP": "True",
                "LITELLM_LOG": "ERROR",
            },
            check=True,
            capture_output=True,
            timeout=120,
        )
        return json.loads((root / "results.json").read_text())


class RelayTests(unittest.TestCase):
    def test_relay_forwards_every_parameter_and_route_upstream(self) -> None:
        results = relay(REQUESTS)
        self.assertEqual(results["startup"], [], "startup must not call the upstream")
        exchanges = cast(list[dict[str, object]], results["exchanges"])
        self.assertEqual(len(exchanges), len(REQUESTS))
        # The one upstream request of each exchange: its URL and JSON body.
        sent: list[dict[str, object]] = []
        for (path, payload), exchange in zip(REQUESTS, exchanges, strict=True):
            upstream = cast(list[dict[str, object]], exchange["upstream"])
            request = upstream[0] if len(upstream) == 1 else {}
            sent.append(
                {"url": request.get("url"), **cast(dict[str, object], request.get("body") or {})}
            )
            with self.subTest(path=path, model=payload["model"]):
                self.assertEqual(exchange["status"], 200, exchange["response"])
                self.assertEqual(len(upstream), 1)
                self.assertEqual(request["method"], "POST")
                self.assertEqual(request["authorization"], "Bearer " + UPSTREAM_KEY)
                # The upstream gateway serves the same deployment under the same name.
                self.assertEqual(sent[-1]["model"], payload["model"])

        def forwarded(index: int, *names: str) -> None:
            for name in names:
                self.assertEqual(sent[index].get(name), REQUESTS[index][1][name], name)

        chat = "http://127.0.0.1:14000/v1/chat/completions"
        responses = "http://127.0.0.1:14000/v1/responses"
        self.assertEqual(sent[0]["url"], chat)
        forwarded(0, "tool_choice", "top_p", "logprobs", "reasoning_effort", "user", "tools")
        forwarded(1, "temperature", "reasoning_effort")
        forwarded(2, "reasoning_effort")
        # A responses-only deployment is bridged to the upstream Responses API.
        self.assertEqual(sent[3]["url"], responses)
        self.assertEqual(sent[4]["url"], responses)
        self.assertEqual(sent[4]["reasoning"], {"effort": "none"})
        self.assertEqual(sent[5]["url"], "http://127.0.0.1:14000/v1/embeddings")
        # Anthropic-format requests for chat deployments stay on chat completions upstream.
        for index in (6, 7):
            self.assertEqual(sent[index]["url"], chat)
        usage = cast(dict[str, object], cast(dict[str, object], exchanges[6]["response"])["usage"])
        self.assertEqual(usage["cache_read_input_tokens"], 2)


if __name__ == "__main__":
    unittest.main()
