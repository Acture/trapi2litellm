"""Exercise the actual installed server without identity requests or inference."""

import argparse
import hashlib
import json
import os
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, Request, build_opener

KEY = "sk-offline-packaging-key"
FETCHED_AT = "2026-10-10T00:00:00Z"


def prepare(config: Path, state: Path) -> None:
    config.mkdir(parents=True, exist_ok=True, mode=0o700)
    state.mkdir(parents=True, exist_ok=True, mode=0o700)
    (config / "gateway.env").write_text(f"LITELLM_MASTER_KEY={KEY}\n")
    (config / "gateway.env").chmod(0o600)
    # A synthetic catalog-only model. No test calls an inference endpoint.
    (config / "config.yaml").write_text(
        json.dumps(
            {
                "model_list": [
                    {
                        "model_name": "trapi/offline",
                        "litellm_params": {
                            "model": "azure/gpt-4o",
                            "api_base": "https://upstream.invalid",
                            "api_version": "2025-04-01-preview",
                        },
                        "model_info": {
                            "capabilities": {"chat_completion": True},
                            "rate_limits": {"requests": {"count": 12}, "tokens": 400},
                        },
                    }
                ],
                "general_settings": {
                    "master_key": "os.environ/LITELLM_MASTER_KEY",
                    "disable_spend_logs": True,
                },
                "litellm_settings": {"telemetry": False, "turn_off_message_logging": True},
            }
        )
        + "\n"
    )
    (state / "catalog.json").write_text(
        json.dumps({"catalog": {"data": []}, "fixture": True, "fetched_at": FETCHED_AT})
    )
    (state / "sync-status.json").write_text(
        json.dumps(
            {
                "checked_at": FETCHED_AT,
                "config_sha256": hashlib.sha256((config / "config.yaml").read_bytes()).hexdigest(),
            }
        )
    )


def seed_usage(state: Path) -> None:
    """Seed numbers in the initialized store; never invoke upstream inference."""
    with sqlite3.connect(state / "usage.sqlite3", timeout=5) as db:
        db.execute("INSERT INTO workers VALUES (?,?)", ("packaging-fixture", time.time()))
        db.execute(
            "INSERT INTO requests VALUES (?,?,?,?,?,?,?,?,?,?)",
            (
                "packaging-fixture",
                "packaging-fixture",
                "trapi/offline",
                time.time(),
                time.time(),
                "succeeded",
                999,
                999,
                12,
                6,
            ),
        )


def wait_ready(
    port: int, process: subprocess.Popen[bytes] | None = None, digest: str | None = None
) -> None:
    client = build_opener(ProxyHandler({}))
    deadline = time.monotonic() + 45
    consecutive = 0
    while time.monotonic() < deadline:
        if process and process.poll() is not None:
            raise RuntimeError("Gateway process exited before becoming ready")
        try:
            with client.open(
                Request(
                    f"http://127.0.0.1:{port}/v1/models", headers={"Authorization": f"Bearer {KEY}"}
                ),
                timeout=2,
            ) as response:
                models = json.load(response)
                if {item["id"] for item in models["data"]} == {"trapi/offline"} and (
                    digest is None or response.headers.get("X-TRAPI-Config-SHA256") == digest
                ):
                    consecutive += 1
                    if consecutive >= 3:
                        return
                else:
                    consecutive = 0
        except (URLError, TimeoutError):
            consecutive = 0  # Worker startup has a bounded readiness deadline.
        time.sleep(0.2)
    raise RuntimeError("Gateway did not expose the fixture model")


def probe(port: int) -> None:
    client = build_opener(ProxyHandler({}))
    for endpoint in ("/v1/models", "/gateway/models"):
        for key in (None, "wrong-key"):
            request = Request(
                f"http://127.0.0.1:{port}{endpoint}",
                headers={"Authorization": f"Bearer {key}"} if key else {},
            )
            try:
                client.open(request, timeout=30).close()
            except HTTPError as error:
                if error.code != 401:
                    raise
            else:
                raise AssertionError("Missing/wrong key was accepted")
    for endpoint in ("/v1/models", "/catalog", "/status", "/model/info", "/gateway/models"):
        with client.open(
            Request(
                f"http://127.0.0.1:{port}{endpoint}", headers={"Authorization": f"Bearer {KEY}"}
            ),
            timeout=30,
        ) as response:
            if not response.headers.get("X-TRAPI-Config-SHA256"):
                raise AssertionError("Missing configuration hash")
            payload = json.load(response)
            if endpoint == "/gateway/models":
                model = payload["data"][0]
                if (
                    model["capabilities"] != {"chat_completion": True}
                    or model["limits"][0]["per_minute"] != 12
                ):
                    raise AssertionError("Installed model metadata was lost")
                if payload["catalog_fetched_at"] != FETCHED_AT:
                    raise AssertionError("Missing catalog fetch time")
                if model["usage"]["tokens"]["reported"] != {"input": 12, "output": 6}:
                    raise AssertionError("Shared usage was lost across workers/reload")
    for endpoint, marker in (
        ("/gateway", b"TRAPI Gateway"),
        ("/gateway/dashboard.js", b"/gateway/models"),
    ):
        with client.open(f"http://127.0.0.1:{port}{endpoint}", timeout=30) as response:
            body = response.read()
            if marker not in body or KEY.encode() in body:
                raise AssertionError("Installed dashboard resource missing or leaked key")


def check(command: list[str], work: Path, config_path: str = "default") -> None:
    config, state = work / "config", work / "state"
    prepare(config, state)
    selected_config: Path = config / "config.yaml"
    override: dict[str, str] = {}
    if config_path != "default":
        selected_config = work / "custom.yaml"
        (config / "config.yaml").rename(selected_config)
        override["CONFIG_FILE_PATH"] = (
            selected_config.name if config_path == "relative" else str(selected_config)
        )
    # A local checkout/PYTHONPATH must not replace the installed runtime,
    # including after its second exec into Gunicorn and after worker reload.
    poison = work / "trapi2litellm"
    poison.mkdir()
    (poison / "__init__.py").write_text('raise RuntimeError("Imported the source fixture")\n')
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env: dict[str, str] = {
        **{key: value for key, value in os.environ.items() if key != "CONFIG_FILE_PATH"},
        "TRAPI2LITELLM_CONFIG_DIR": str(config),
        "TRAPI2LITELLM_STATE_DIR": str(state),
        "LITELLM_LOCAL_MODEL_COST_MAP": "True",
        "LITELLM_MASTER_KEY": KEY,
        "TRAPI2LITELLM_PYTHON": "",
        "PYTHONPATH": str(work),
        **override,
    }
    with (work / "gateway.log").open("wb") as log:
        process = subprocess.Popen(
            [*command, "serve", "--port", str(port)], cwd=work, env=env, stdout=log, stderr=log
        )
        try:
            wait_ready(port, process)
            seed_usage(state)
            probe(port)
            with selected_config.open("a") as handle:
                handle.write("# Offline reload acceptance\n")
            digest = hashlib.sha256(selected_config.read_bytes()).hexdigest()
            (state / "sync-status.json").write_text(
                json.dumps({"checked_at": FETCHED_AT, "config_sha256": digest})
            )
            process.send_signal(signal.SIGHUP)
            wait_ready(port, process, digest)
            probe(port)
        except (RuntimeError, URLError, TimeoutError, AssertionError):
            print((work / "gateway.log").read_text())
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command")
    parser.add_argument(
        "--config-path", choices=("default", "relative", "absolute"), default="default"
    )
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="trapi2litellm-gateway-") as folder:
        check([args.command], Path(folder), args.config_path)
    print(
        "Accepted installed gateway startup, auth, metadata endpoints and HUP "
        f"({args.config_path} configuration path; no inference)"
    )


if __name__ == "__main__":
    main()
