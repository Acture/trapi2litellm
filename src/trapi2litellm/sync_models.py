"""Discover TRAPI deployments and safely publish one local LiteLLM config."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import logging
import os
import re
import secrets
import subprocess
import sys
import tempfile
import time
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlencode

import httpx
import yaml
from azure.identity import ManagedIdentityCredential

from trapi2litellm.settings import (
    API_VERSION,
    BASE_URL,
    CATALOG_VERSION,
    CLIENT_ID,
    CONFIG_DIR,
    CONFIG_PATH,
    KEY_PATH,
    LOCAL_URL,
    SCOPE,
    SERVICE,
    STATE_DIR,
)

CATALOG_URL = BASE_URL + "/openai/models?" + urlencode({"api-version": CATALOG_VERSION})


def now() -> str:
    return datetime.now(timezone.utc).isoformat()


def atomic_write(path: Path, content: str) -> None:
    """Replace only this generated file, never expose a partial config."""
    fd, temporary = tempfile.mkstemp(prefix="." + path.name, dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def setup_directories() -> None:
    for path in (CONFIG_DIR, STATE_DIR):
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        path.chmod(0o700)


def bootstrap_key() -> None:
    """Create a local gateway key once; never print or rotate an existing key."""
    if not KEY_PATH.exists():
        try:
            fd = os.open(KEY_PATH, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            return
        with os.fdopen(fd, "w") as handle:
            handle.write("LITELLM_MASTER_KEY=sk-trapi-" + secrets.token_urlsafe(36) + "\n")
    KEY_PATH.chmod(0o600)


def local_key() -> str:
    for line in KEY_PATH.read_text().splitlines():
        if line.startswith("LITELLM_MASTER_KEY="):
            return line.partition("=")[2]
    raise ValueError("gateway.env has no local master key")


def fetch_catalog() -> dict:
    with ManagedIdentityCredential(client_id=CLIENT_ID) as credential:
        token = credential.get_token(SCOPE).token
        with httpx.Client(timeout=30, follow_redirects=False, trust_env=False) as client:
            response = client.get(CATALOG_URL, headers={"Authorization": "Bearer " + token})
            response.raise_for_status()
            return response.json()


def enabled(capabilities: dict, key: str) -> bool:
    return str(capabilities.get(key, "")).lower() == "true"


def build_config(catalog: dict) -> dict:
    if not isinstance(catalog, dict):
        raise ValueError("Catalog must be an object")
    if any(catalog.get(key) for key in ("nextLink", "next_link", "@odata.nextLink", "next")):
        raise ValueError("Paginated catalog requires support before publishing a partial list")
    entries = catalog.get("data")
    if not isinstance(entries, list) or not entries:
        raise ValueError("Empty or malformed catalog; keeping previous configuration")
    models = []
    seen = set()
    for entry in entries:
        if not isinstance(entry, dict):
            raise ValueError("Catalog contains a non-object entry")
        name = entry.get("id")
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]*", name):
            raise ValueError("Catalog contains an invalid deployment ID")
        if ".." in name.split("/") or name in seen:
            raise ValueError("Catalog contains a duplicate or unsafe deployment ID")
        seen.add(name)
        if entry.get("provisioningState") != "Succeeded":
            continue
        capabilities = entry.get("capabilities") or {}
        if not isinstance(capabilities, dict):
            raise ValueError("Malformed model capabilities")
        model_info = {
            "id": hashlib.sha256((BASE_URL + "/" + name).encode()).hexdigest(),
            "source": "trapi",
            "upstream_deployment": name,
            "provisioning_state": entry["provisioningState"],
            "capabilities": capabilities,
            "upstream_model": entry.get("model"),
            "rate_limits": entry.get("RateLimits"),
            "availability_evidence": "catalog_only_not_an_inference_health_check",
        }
        upstream_model = entry.get("model") or {}
        if not isinstance(upstream_model, dict):
            raise ValueError("Malformed model metadata")
        # Deployment IDs include TRAPI's version suffix. Tell LiteLLM the real
        # OpenAI family so parameter validation does not misclassify that ID.
        if upstream_model.get("Format") == "OpenAI" and upstream_model.get("Name"):
            model_info["base_model"] = "azure/" + upstream_model["Name"]
        if enabled(capabilities, "chatCompletion"):
            model_info["mode"] = "chat"
        elif enabled(capabilities, "responses"):
            model_info["mode"] = "responses"
        elif enabled(capabilities, "embeddings"):
            model_info["mode"] = "embedding"
        models.append(
            {
                "model_name": "trapi/" + name,
                "litellm_params": {
                    "model": "azure/" + name,
                    "api_base": BASE_URL,
                    "api_version": API_VERSION,
                    "azure_scope": SCOPE,
                },
                "model_info": model_info,
            }
        )
    if not models:
        raise ValueError("No provisioned deployments; keeping previous configuration")
    return {
        "model_list": sorted(models, key=lambda item: item["model_name"]),
        "litellm_settings": {
            "enable_azure_ad_token_refresh": True,
            "drop_params": False,
            "telemetry": False,
            "set_verbose": False,
            "turn_off_message_logging": True,
        },
        "router_settings": {
            "num_retries": 0,
            "timeout": 600,
            "fallbacks": [],
        },
        "general_settings": {
            "master_key": "os.environ/LITELLM_MASTER_KEY",
            "disable_spend_logs": True,
        },
    }


def validate_config(config: dict, old: dict | None) -> None:
    from litellm.types.router import Deployment

    for entry in config["model_list"]:
        Deployment(**entry)
    if old:
        previous_count = len(old.get("model_list", []))
        if len(config["model_list"]) < previous_count * 0.75:
            raise ValueError("Catalog shrank by over 25%; manual review required, old config kept")


def service_active() -> bool:
    return (
        subprocess.run(
            ["systemctl", "--user", "is-active", "--quiet", SERVICE],
            check=False,
        ).returncode
        == 0
    )


def reload_service() -> None:
    subprocess.run(["systemctl", "--user", "reload", SERVICE], check=True, timeout=15)


def wait_for_models(expected: set[str], expected_digest: str) -> None:
    deadline = time.monotonic() + 90
    consecutive = 0
    with httpx.Client(timeout=5, trust_env=False) as client:
        while time.monotonic() < deadline:
            try:
                response = client.get(
                    LOCAL_URL + "/v1/models",
                    headers={
                        "Authorization": "Bearer " + local_key(),
                        "Connection": "close",
                    },
                )
                response.raise_for_status()
                found = {entry["id"] for entry in response.json()["data"]}
                matches = (
                    found == expected
                    and response.headers.get(
                        "x-trapi-config-sha256",
                    )
                    == expected_digest
                )
                consecutive = consecutive + 1 if matches else 0
                if consecutive >= 3:
                    return
            except (httpx.HTTPError, KeyError, ValueError):
                consecutive = 0
            time.sleep(1)
    raise RuntimeError("Gateway did not expose the expected model list within 90 seconds")


def sync(bootstrap: bool = False, no_reload: bool = False) -> dict:
    setup_directories()
    if bootstrap:
        bootstrap_key()
    with (STATE_DIR / "sync.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        catalog = fetch_catalog()
        config = build_config(catalog)
        old_text = CONFIG_PATH.read_text() if CONFIG_PATH.exists() else None
        old = yaml.safe_load(old_text) if old_text else None
        validate_config(config, old)
        rendered = "# Generated by sync_models.py; do not edit.\n" + yaml.safe_dump(
            config,
            sort_keys=True,
            allow_unicode=True,
        )
        changed = rendered != old_text
        digest = hashlib.sha256(rendered.encode()).hexdigest()
        atomic_write(
            STATE_DIR / "catalog.json",
            json.dumps(
                {
                    "fetched_at": now(),
                    "source": CATALOG_URL,
                    "catalog": catalog,
                },
                indent=2,
            )
            + "\n",
        )
        result = {
            "checked_at": now(),
            "source_entries": len(catalog["data"]),
            "configured_models": len(config["model_list"]),
            "config_sha256": digest,
            "changed": changed,
            "reloaded": False,
        }
        if changed:
            if old_text:
                atomic_write(STATE_DIR / "config.previous.yaml", old_text)
            atomic_write(CONFIG_PATH, rendered)
            try:
                if not no_reload and service_active():
                    reload_service()
                    wait_for_models(
                        {entry["model_name"] for entry in config["model_list"]},
                        digest,
                    )
                    result["reloaded"] = True
            except Exception:
                atomic_write(STATE_DIR / "config.rejected.yaml", rendered)
                if old_text:
                    atomic_write(CONFIG_PATH, old_text)
                    reload_service()
                raise
        result["status"] = "ok"
        atomic_write(STATE_DIR / "sync-status.json", json.dumps(result, indent=2) + "\n")
        return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="trapi2litellm sync", description=__doc__)
    parser.add_argument("--bootstrap", action="store_true")
    parser.add_argument("--no-reload", action="store_true")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.WARNING)
    try:
        result = sync(bootstrap=args.bootstrap, no_reload=args.no_reload)
    except Exception as exc:
        # Credential/HTTP exceptions may contain request details: do not dump them.
        result = {"status": "error", "checked_at": now(), "error_type": type(exc).__name__}
        if isinstance(exc, ValueError):
            result["message"] = str(exc)
        if isinstance(exc, httpx.HTTPStatusError):
            result["http_status"] = exc.response.status_code
        if STATE_DIR.exists():
            atomic_write(STATE_DIR / "sync-error.json", json.dumps(result, indent=2) + "\n")
        print(json.dumps(result), flush=True)
        return 1
    print(json.dumps(result), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
