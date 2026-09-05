"""Portable paths and explicit service settings; no credentials in source."""

import os
from pathlib import Path
from urllib.parse import urlsplit


def directory(variable: str, fallback: Path) -> Path:
    return Path(os.environ.get(variable, str(fallback))).expanduser().resolve()


CONFIG_DIR = directory(
    "TRAPI2LITELLM_CONFIG_DIR",
    directory(
        "XDG_CONFIG_HOME",
        Path.home() / ".config",
    )
    / "litellm-trapi",
)
STATE_DIR = directory(
    "TRAPI2LITELLM_STATE_DIR",
    directory(
        "XDG_STATE_HOME",
        Path.home() / ".local" / "state",
    )
    / "trapi2litellm",
)
CONFIG_PATH = CONFIG_DIR / "config.yaml"
KEY_PATH = CONFIG_DIR / "gateway.env"
BASE_URL = os.environ.get(
    "TRAPI_BASE_URL",
    "https://trapi.research.microsoft.com/redmond/interactive",
).rstrip("/")
parsed = urlsplit(BASE_URL)
if (
    parsed.scheme != "https"
    or not parsed.hostname
    or parsed.username
    or parsed.query
    or parsed.fragment
):
    raise ValueError(
        "TRAPI_BASE_URL must be an HTTPS base URL without credentials, query or fragment"
    )
API_VERSION = os.environ.get("TRAPI_API_VERSION", "2025-04-01-preview")
CATALOG_VERSION = os.environ.get("TRAPI_CATALOG_VERSION", "preview")
SCOPE = os.environ.get("TRAPI_SCOPE", "api://trapi/.default")
CLIENT_ID = os.environ.get("AZURE_CLIENT_ID") or None
PORT = int(os.environ.get("TRAPI2LITELLM_PORT", "4000"))
if not 1024 <= PORT <= 65535:
    raise ValueError("TRAPI2LITELLM_PORT must be between 1024 and 65535")
LOCAL_URL = f"http://127.0.0.1:{PORT}"
SERVICE = "litellm-trapi.service"
