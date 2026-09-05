"""Small live gateway checks. Sends bounded synthetic prompts, never prints keys."""

import json
import subprocess
from datetime import datetime, timezone

import httpx

from sync_models import LOCAL_URL, SERVICE, STATE_DIR, atomic_write, local_key


def main():
    checks = []

    def record(name, **details):
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

        client.headers["Authorization"] = "Bearer " + local_key()
        for path in ["/v1/models", "/catalog", "/status", "/model/info"]:
            response = client.get(path)
            response.raise_for_status()
            assert response.headers.get("x-trapi-config-sha256")
            details = {}
            if path == "/v1/models":
                details["models"] = len(response.json()["data"])
                assert "trapi/gpt-5.2_2025-12-11" in {x["id"] for x in response.json()["data"]}
            record(path, status=response.status_code, **details)

        def post(path, payload):
            response = client.post(path, json=payload)
            if not response.is_success:
                error = response.json().get("error", {})
                message = error.get("message", "") if isinstance(error, dict) else str(error)
                message = message.replace(local_key(), "[REDACTED]")
                raise RuntimeError(f"{path}: HTTP {response.status_code}: {message[:500]}")
            return response.json()

        data = post(
            "/v1/chat/completions",
            {
                "model": "trapi/gpt-5.2_2025-12-11",
                "messages": [{"role": "user", "content": "Reply with only OK."}],
                "max_completion_tokens": 128,
                "reasoning_effort": "none",
            },
        )
        assert data["choices"][0]["message"]["content"].strip() == "OK"
        record("gpt52_chat", model=data["model"], usage=data.get("usage"))

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
        assert data["choices"][0]["message"]["content"].strip() == "5"
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

        master_before = subprocess.check_output(
            ["systemctl", "--user", "show", SERVICE, "-p", "MainPID", "--value"],
            text=True,
        ).strip()
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
                        subprocess.run(["systemctl", "--user", "reload", SERVICE], check=True)
                        reloaded = True
        assert reloaded and done and "100" in text
        master_after = subprocess.check_output(
            ["systemctl", "--user", "show", SERVICE, "-p", "MainPID", "--value"],
            text=True,
        ).strip()
        assert master_after == master_before
        record("reload_during_stream", stream_completed=done, master_pid_unchanged=True)

    report = {"tested_at": datetime.now(timezone.utc).isoformat(), "checks": checks}
    atomic_write(STATE_DIR / "smoke-test.json", json.dumps(report, indent=2) + "\n")
    print("All live checks passed.", flush=True)


if __name__ == "__main__":
    main()
