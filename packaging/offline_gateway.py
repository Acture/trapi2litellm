"""Exercise the actual installed server without identity requests or inference."""

import argparse
import hashlib
import json
import os
import signal
import socket
import subprocess
import tempfile
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, Request, build_opener

KEY = "sk-offline-packaging-key"


def prepare(config: Path, state: Path) -> None:
    config.mkdir(parents=True, exist_ok=True, mode=0o700)
    state.mkdir(parents=True, exist_ok=True, mode=0o700)
    (config / "gateway.env").write_text(f"LITELLM_MASTER_KEY={KEY}\n")
    (config / "gateway.env").chmod(0o600)
    # A synthetic catalog-only model. No test calls an inference endpoint.
    (config / "config.yaml").write_text(
        "model_list:\n  - model_name: trapi/offline\n    litellm_params:\n      model: azure/gpt-4o\n      api_base: https://upstream.invalid\n      api_version: '2025-04-01-preview'\ngeneral_settings:\n  master_key: os.environ/LITELLM_MASTER_KEY\n  disable_spend_logs: true\nlitellm_settings:\n  telemetry: false\n  turn_off_message_logging: true\n"
    )
    (state / "catalog.json").write_text('{"catalog": {"data": []}, "fixture": true}\n')


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
    for key in (None, "wrong-key"):
        request = Request(
            f"http://127.0.0.1:{port}/v1/models",
            headers={"Authorization": f"Bearer {key}"} if key else {},
        )
        try:
            client.open(request, timeout=30).close()
        except HTTPError as error:
            if error.code != 401:
                raise
        else:
            raise AssertionError("Missing/wrong key was accepted")
    for endpoint in ("/v1/models", "/catalog", "/status", "/model/info"):
        with client.open(
            Request(
                f"http://127.0.0.1:{port}{endpoint}", headers={"Authorization": f"Bearer {KEY}"}
            ),
            timeout=30,
        ) as response:
            if not response.headers.get("X-TRAPI-Config-SHA256"):
                raise AssertionError("Missing configuration hash")
            json.load(response)


def check(command: list[str], work: Path) -> None:
    config, state = work / "config", work / "state"
    prepare(config, state)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = {
        **os.environ,
        "TRAPI2LITELLM_CONFIG_DIR": str(config),
        "TRAPI2LITELLM_STATE_DIR": str(state),
        "LITELLM_LOCAL_MODEL_COST_MAP": "True",
        "LITELLM_MASTER_KEY": KEY,
    }
    with (work / "gateway.log").open("wb") as log:
        process = subprocess.Popen(
            [*command, "serve", "--port", str(port)], cwd=work, env=env, stdout=log, stderr=log
        )
        try:
            wait_ready(port, process)
            probe(port)
            with (config / "config.yaml").open("a") as handle:
                handle.write("# Offline reload acceptance\n")
            digest = hashlib.sha256((config / "config.yaml").read_bytes()).hexdigest()
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
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="trapi2litellm-gateway-") as folder:
        check([args.command], Path(folder))
    print("Accepted installed gateway startup, auth, metadata endpoints and HUP (no inference)")


if __name__ == "__main__":
    main()
