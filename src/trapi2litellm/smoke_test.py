"""Small live gateway checks. Sends bounded synthetic prompts, never prints keys."""

import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import TypedDict, cast

import httpx

LOCAL_URL: str = "http://127.0.0.1:" + os.environ["TRAPI2LITELLM_PORT"]
SERVICE: str = "litellm-trapi.service"
STATE_DIR: Path = Path(os.environ["TRAPI2LITELLM_STATE_DIR"])
# The native command passes launchd and the gateway label on macOS.
SERVICE_MANAGER: str = os.environ.get("TRAPI2LITELLM_SERVICE_MANAGER", "systemd")
LAUNCHCTL: str = "/bin/launchctl"
LAUNCHCTL_TIMEOUT: int = 60


class ToolFunction(TypedDict):
    name: str
    arguments: str


class ToolCall(TypedDict):
    id: str
    function: ToolFunction


class ChatMessage(TypedDict, total=False):
    content: str | None
    tool_calls: list[ToolCall]


class ChatChoice(TypedDict):
    message: ChatMessage


class ResponsePart(TypedDict):
    type: str
    text: str


class ResponseItem(TypedDict):
    content: list[ResponsePart]


class ApiResponse(TypedDict, total=False):
    choices: list[ChatChoice]
    output: list[ResponseItem]
    model: str
    usage: dict[str, object]


def response_text(response: ApiResponse) -> str:
    content = response["choices"][0]["message"]["content"]
    assert isinstance(content, str)
    return content.strip()


def launchd_target() -> str:
    return f"gui/{os.getuid()}/{os.environ['TRAPI2LITELLM_SERVICE_LABEL']}"


def launchd_pid(output: str) -> str:
    """Process ID from `launchctl print`; nested blocks are indented further."""
    pids = [
        line.removeprefix("\tpid = ") for line in output.splitlines() if line.startswith("\tpid = ")
    ]
    if len(pids) != 1 or not pids[0].isdigit():
        raise RuntimeError("launchctl print did not report one running gateway process")
    return pids[0]


def gateway_pid() -> str:
    if SERVICE_MANAGER == "launchd":
        output = subprocess.check_output(
            [LAUNCHCTL, "print", launchd_target()], text=True, timeout=LAUNCHCTL_TIMEOUT
        )
        return launchd_pid(output)
    return subprocess.check_output(
        ["systemctl", "--user", "show", SERVICE, "-p", "MainPID", "--value"],
        text=True,
    ).strip()


def reload_gateway() -> None:
    if SERVICE_MANAGER == "launchd":
        subprocess.run(
            [LAUNCHCTL, "kill", "SIGHUP", launchd_target()],
            check=True,
            timeout=LAUNCHCTL_TIMEOUT,
        )
    else:
        subprocess.run(["systemctl", "--user", "reload", SERVICE], check=True)


def main() -> None:
    if SERVICE_MANAGER not in ("systemd", "launchd"):
        raise ValueError(f"Unsupported service manager {SERVICE_MANAGER!r}")
    # Fail before any billable check rather than at the reload check.
    if SERVICE_MANAGER == "launchd" and not os.environ.get("TRAPI2LITELLM_SERVICE_LABEL"):
        raise ValueError("TRAPI2LITELLM_SERVICE_LABEL is required with launchd")
    key: str = os.environ["LITELLM_MASTER_KEY"]
    checks = []

    def record(name: str, **details: object) -> None:
        item = {"check": name, "passed": True, **details}
        checks.append(item)
        print(json.dumps(item), flush=True)

    with httpx.Client(base_url=LOCAL_URL, timeout=90, trust_env=False) as client:
        for label, headers in [
            ("missing_key", {}),
            ("wrong_key", {"Authorization": "Bearer sk-invalid"}),
        ]:
            response = client.get("/v1/models", headers=headers)
            assert response.status_code == 401, (label, response.status_code)
            record(label, status=response.status_code)

        client.headers["Authorization"] = "Bearer " + key
        for path in ["/v1/models", "/catalog", "/status", "/model/info"]:
            response = client.get(path)
            response.raise_for_status()
            assert response.headers.get("x-trapi-config-sha256")
            details = {}
            if path == "/v1/models":
                details["models"] = len(response.json()["data"])
                assert "trapi/gpt-5.2_2025-12-11" in {x["id"] for x in response.json()["data"]}
            record(path, status=response.status_code, **details)

        def post(path: str, payload: dict[str, object]) -> ApiResponse:
            response = client.post(path, json=payload)
            if not response.is_success:
                error = response.json().get("error", {})
                message = error.get("message", "") if isinstance(error, dict) else str(error)
                message = message.replace(key, "[REDACTED]")
                raise RuntimeError(f"{path}: HTTP {response.status_code}: {message[:500]}")
            return cast(ApiResponse, response.json())

        data = post(
            "/v1/chat/completions",
            {
                "model": "trapi/gpt-5.2_2025-12-11",
                "messages": [{"role": "user", "content": "Reply with only OK."}],
                "max_completion_tokens": 128,
                "reasoning_effort": "none",
            },
        )
        assert response_text(data) == "OK"
        record("gpt52_chat", model=data["model"], usage=data.get("usage"))

        tools = [
            {
                "type": "function",
                "function": {
                    "name": "add",
                    "description": "Add two integers.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "a": {"type": "integer"},
                            "b": {"type": "integer"},
                        },
                        "required": ["a", "b"],
                        "additionalProperties": False,
                    },
                },
            }
        ]
        # A gateway that maps parameters for an unknown model can drop tool_choice silently.
        data = post(
            "/v1/chat/completions",
            {
                "model": "trapi/gpt-5.2_2025-12-11",
                "messages": [{"role": "user", "content": "Use add to calculate 2 + 3."}],
                "tools": tools,
                "tool_choice": {"type": "function", "function": {"name": "add"}},
                "max_completion_tokens": 128,
                "reasoning_effort": "none",
            },
        )
        calls = data["choices"][0]["message"]["tool_calls"]
        assert len(calls) == 1 and calls[0]["function"]["name"] == "add"
        record("gpt52_forced_tool_call", model=data["model"])

        text = ""
        done = False
        with client.stream(
            "POST",
            "/v1/chat/completions",
            json={
                "model": "trapi/gpt-4o-mini_2024-07-18",
                "messages": [{"role": "user", "content": "Reply with only OK."}],
                "max_tokens": 16,
                "stream": True,
            },
        ) as response:
            response.raise_for_status()
            for line in response.iter_lines():
                if line == "data: [DONE]":
                    done = True
                elif line.startswith("data: "):
                    chunk = json.loads(line[6:])
                    for choice in chunk.get("choices", []):
                        text += choice.get("delta", {}).get("content") or ""
        assert text.strip() == "OK" and done
        record("streaming", output=text, done=done)

        messages = [
            {
                "role": "user",
                "content": "Use add to calculate 2 + 3. Then reply with only the result.",
            }
        ]
        data = post(
            "/v1/chat/completions",
            {
                "model": "trapi/gpt-4o-mini_2024-07-18",
                "messages": messages,
                "tools": tools,
                "tool_choice": {"type": "function", "function": {"name": "add"}},
                "max_tokens": 128,
            },
        )
        calls = data["choices"][0]["message"]["tool_calls"]
        assert len(calls) == 1 and calls[0]["function"]["name"] == "add"
        arguments = json.loads(calls[0]["function"]["arguments"])
        assert arguments == {"a": 2, "b": 3}
        messages.append({"role": "assistant", "content": None, "tool_calls": calls})
        messages.append({"role": "tool", "tool_call_id": calls[0]["id"], "content": "5"})
        data = post(
            "/v1/chat/completions",
            {
                "model": "trapi/gpt-4o-mini_2024-07-18",
                "messages": messages,
                "tools": tools,
                "max_tokens": 32,
            },
        )
        assert response_text(data) == "5"
        record("tool_call_roundtrip", output="5")

        data = post(
            "/v1/responses",
            {
                "model": "trapi/gpt-5.2_2025-12-11",
                "input": "Reply with only OK.",
                "max_output_tokens": 128,
                "reasoning": {"effort": "none"},
                "store": False,
            },
        )
        text = "".join(
            part.get("text", "")
            for item in data.get("output", [])
            for part in item.get("content", [])
            if part.get("type") == "output_text"
        )
        assert text.strip() == "OK"
        record("responses_api", output=text)

        master_before = gateway_pid()
        reloaded = False
        done = False
        text = ""
        with client.stream(
            "POST",
            "/v1/chat/completions",
            json={
                "model": "trapi/gpt-4o-mini_2024-07-18",
                "messages": [
                    {
                        "role": "user",
                        "content": "Count from 1 to 100, separated by spaces. No explanation.",
                    }
                ],
                "max_tokens": 400,
                "stream": True,
            },
        ) as response:
            response.raise_for_status()
            for line in response.iter_lines():
                if line == "data: [DONE]":
                    done = True
                elif line.startswith("data: "):
                    chunk = json.loads(line[6:])
                    for choice in chunk.get("choices", []):
                        text += choice.get("delta", {}).get("content") or ""
                    if text and not reloaded:
                        reload_gateway()
                        reloaded = True
        assert reloaded and done and "100" in text
        master_after = gateway_pid()
        assert master_after == master_before
        record("reload_during_stream", stream_completed=done, master_pid_unchanged=True)

    report = {"tested_at": datetime.now(timezone.utc).isoformat(), "checks": checks}
    (STATE_DIR / "smoke-test.json").write_text(json.dumps(report, indent=2) + "\n")
    print("All live checks passed.", flush=True)


if __name__ == "__main__":
    main()
