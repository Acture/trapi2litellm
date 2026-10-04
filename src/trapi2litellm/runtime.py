"""Internal SDK/schema and gateway boundary used by the native Rust command."""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
from contextlib import redirect_stdout
from pathlib import Path
from typing import cast
from urllib.parse import urlencode

import httpx
import yaml


def object_value(value: object) -> dict[str, object]:
    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise ValueError("Expected a JSON object")
    return cast(dict[str, object], value)


def string_value(payload: dict[str, object], name: str) -> str:
    value = payload.get(name)
    if not isinstance(value, str) or not value:
        raise ValueError("Missing string field: " + name)
    return value


def catalog(payload: dict[str, object]) -> dict[str, object]:
    from azure.identity import ManagedIdentityCredential

    client_id = payload.get("client_id")
    if client_id is not None and not isinstance(client_id, str):
        raise ValueError("Invalid client_id")
    url = (
        string_value(payload, "base_url")
        + "/openai/models?"
        + urlencode({"api-version": string_value(payload, "catalog_version")})
    )
    with ManagedIdentityCredential(client_id=client_id or None) as credential:
        token = credential.get_token(string_value(payload, "scope")).token
        with httpx.Client(timeout=30, follow_redirects=False, trust_env=False) as client:
            response = client.get(url, headers={"Authorization": "Bearer " + token})
            response.raise_for_status()
            return object_value(response.json())


def validate(payload: dict[str, object]) -> dict[str, object]:
    from litellm.types.router import Deployment

    config = object_value(payload.get("config"))
    models = config.get("model_list")
    if not isinstance(models, list) or not models:
        raise ValueError("Expected nonempty model_list")
    for entry in models:
        Deployment.model_validate(object_value(entry))
    old = payload.get("previous_text")
    if old is not None and not isinstance(old, str):
        raise ValueError("Invalid previous_text")
    previous: list[str] = []
    if old:
        previous_config = object_value(yaml.safe_load(old))
        previous_models = previous_config.get("model_list", [])
        if not isinstance(previous_models, list):
            raise ValueError("Invalid previous model_list")
        for entry in previous_models:
            previous.append(string_value(object_value(entry), "model_name"))
    return {"previous_models": previous}


def serve() -> None:
    """Replace the SDK boundary process with Gunicorn, preserving PID/HUP."""
    package_root: Path = Path(__file__).resolve().parent.parent
    # Gunicorn inserts its initial cwd before it processes --chdir.
    os.chdir(package_root)
    command: list[str] = [
        sys.executable,
        *(["-I"] if sys.flags.isolated else []),
        "-m",
        "gunicorn",
        "trapi2litellm.gateway_app:app",
        "--chdir",
        str(package_root),
        "--bind",
        "127.0.0.1:" + os.environ["TRAPI2LITELLM_PORT"],
        "--workers",
        "2",
        "--worker-class",
        "uvicorn_worker.UvicornWorker",
        "--timeout",
        "120",
        "--graceful-timeout",
        "900",
        "--keep-alive",
        "5",
        "--error-logfile",
        "-",
        "--log-level",
        "warning",
    ]
    os.execv(sys.executable, command)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("catalog", "validate", "serve", "smoke-test"))
    args = parser.parse_args()
    logging.basicConfig(level=logging.WARNING)
    if args.operation == "serve":
        serve()
        return 0
    if args.operation == "smoke-test":
        from trapi2litellm.smoke_test import main as smoke_test

        smoke_test()
        return 0
    try:
        payload = object_value(json.load(sys.stdin))
        # Third-party initialization output must not corrupt the JSON protocol.
        with redirect_stdout(sys.stderr):
            result = catalog(payload) if args.operation == "catalog" else validate(payload)
    except Exception as error:
        # System boundary: SDK exceptions can contain a token or request URL.
        failure: dict[str, object] = {"error_type": type(error).__name__}
        if isinstance(error, httpx.HTTPStatusError):
            failure["http_status"] = error.response.status_code
        print(json.dumps(failure), flush=True)
        return 1
    print(json.dumps(result), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
