"""Install/update user-level systemd services without placing secrets in Git."""

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

SOURCE = Path(__file__).resolve().parent
MARKER = "# Managed by trapi2litellm\n"


def unit_quote(value: str | Path) -> str:
    text = str(value)
    if any(ord(char) < 32 for char in text):
        raise ValueError("Control characters are not supported in service settings")
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'


def managed_write(path: Path, content: str, *, legacy: str | None = None) -> bool:
    """Keep one recoverable backup and refuse to overwrite unrelated units."""
    from sync_models import atomic_write

    if path.exists():
        old = path.read_text()
        if old == content:
            return False
        if not old.startswith(MARKER) and not (legacy and legacy in old):
            raise ValueError(f"Refusing to overwrite unmanaged file: {path}")
        atomic_write(path.with_suffix(path.suffix + ".previous"), old)
    atomic_write(path, content)
    return True


def render_units(
    source: Path, config_dir: Path, state_dir: Path, port: int, options: dict[str, str]
) -> dict[str, str]:
    q = unit_quote
    python = source / ".venv/bin/python"
    gunicorn = source / ".venv/bin/gunicorn"
    environments = {
        "TRAPI2LITELLM_CONFIG_DIR": str(config_dir),
        "TRAPI2LITELLM_STATE_DIR": str(state_dir),
        "TRAPI2LITELLM_PORT": str(port),
        "LITELLM_LOCAL_MODEL_COST_MAP": "True",
        "PYTHONDONTWRITEBYTECODE": "1",
        "NO_PROXY": "127.0.0.1,localhost,169.254.169.254",
        "no_proxy": "127.0.0.1,localhost,169.254.169.254",
        **options,
    }
    shared = "".join(
        f"Environment={q(key + '=' + value)}\n" for key, value in sorted(environments.items())
    )
    gateway = (
        MARKER
        + f"""[Unit]
Description=Local TRAPI LiteLLM gateway (Managed Identity)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
WorkingDirectory={str(source).replace("%", "%%")}
EnvironmentFile={str(config_dir / "gateway.env").replace("%", "%%")}
{shared}Environment={q("CONFIG_FILE_PATH=" + str(config_dir / "config.yaml"))}
Environment=LITELLM_MODE=PRODUCTION
Environment=LITELLM_LOG=WARNING
Environment=AZURE_TOKEN_CREDENTIALS=ManagedIdentityCredential
Environment=AZURE_CREDENTIAL=DefaultAzureCredential
ExecStart={q(gunicorn)} gateway_app:app --bind 127.0.0.1:{port} --workers 2 --worker-class uvicorn_worker.UvicornWorker --timeout 120 --graceful-timeout 900 --keep-alive 5 --error-logfile - --log-level warning
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5
TimeoutStopSec=930
KillMode=mixed
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
"""
    )
    sync = (
        MARKER
        + f"""[Unit]
Description=Discover TRAPI models and update the local LiteLLM gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
WorkingDirectory={str(source).replace("%", "%%")}
{shared}ExecStart={q(python)} {q(source / "sync_models.py")}
TimeoutStartSec=240
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
"""
    )
    timer = (
        MARKER
        + """[Unit]
Description=Refresh TRAPI model catalog every hour

[Timer]
OnCalendar=hourly
RandomizedDelaySec=120
Persistent=true
Unit=litellm-trapi-sync.service

[Install]
WantedBy=timers.target
"""
    )
    return {
        "litellm-trapi.service": gateway,
        "litellm-trapi-sync.service": sync,
        "litellm-trapi-sync.timer": timer,
    }


def write_client_files(config_dir: Path, port: int) -> None:
    from sync_models import atomic_write

    key_path = config_dir / "gateway.env"
    atomic_write(
        config_dir / "client.sh",
        MARKER + f". {shlex.quote(str(key_path))}\n"
        f"export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1\n"
        'export OPENAI_API_KEY="$LITELLM_MASTER_KEY"\n',
    )

    fish_path = str(key_path).replace("\\", "\\\\").replace("'", "\\'")
    atomic_write(
        config_dir / "client.fish",
        MARKER + f"set -gx OPENAI_BASE_URL http://127.0.0.1:{port}/v1\n"
        "set -gx OPENAI_API_KEY (string replace 'LITELLM_MASTER_KEY=' '' < "
        f"'{fish_path}')\n",
    )


def preflight_units(units: dict[str, str]) -> None:
    """Validate rendered units before replacing any live unit files."""
    with tempfile.TemporaryDirectory(prefix="trapi2litellm-units-") as folder:
        paths = []
        for name, content in units.items():
            path = Path(folder) / name
            path.write_text(content)
            paths.append(str(path))
        result = subprocess.run(
            ["systemd-analyze", "--user", "verify", *paths],
            check=True,
            capture_output=True,
            text=True,
        )
        if "path is not absolute" in result.stderr:
            raise ValueError("systemd rejected a rendered path")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config-dir", type=Path)
    parser.add_argument("--state-dir", type=Path)
    parser.add_argument("--port", type=int)
    parser.add_argument(
        "--enable-linger", action="store_true", help="Keep user services alive after logout"
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="Print units only; no writes or network calls"
    )
    args = parser.parse_args()
    for key, value in [
        ("TRAPI2LITELLM_CONFIG_DIR", args.config_dir),
        ("TRAPI2LITELLM_STATE_DIR", args.state_dir),
        ("TRAPI2LITELLM_PORT", args.port),
    ]:
        if value is not None:
            os.environ[key] = str(value)
    import settings
    import sync_models

    # Keep secrets out of units: these are endpoint/identity selectors only.
    options = {
        "TRAPI_BASE_URL": settings.BASE_URL,
        "TRAPI_API_VERSION": settings.API_VERSION,
        "TRAPI_CATALOG_VERSION": settings.CATALOG_VERSION,
        "TRAPI_SCOPE": settings.SCOPE,
    }
    if settings.CLIENT_ID:
        options["AZURE_CLIENT_ID"] = settings.CLIENT_ID
    for runtime_path in (settings.CONFIG_DIR, settings.STATE_DIR):
        if runtime_path == SOURCE or SOURCE in runtime_path.parents:
            raise ValueError("Configuration and state must live outside the source checkout")
    units = render_units(SOURCE, settings.CONFIG_DIR, settings.STATE_DIR, settings.PORT, options)
    if args.dry_run:
        for name, content in units.items():
            print(f"# {name}\n{content}")
        return 0
    if not sys.platform.startswith("linux"):
        raise ValueError("Deployment currently supports Linux with systemd --user only")
    for executable in (SOURCE / ".venv/bin/gunicorn", SOURCE / ".venv/bin/python"):
        if not executable.is_file():
            raise ValueError("Run uv sync --frozen in the checkout first")
    subprocess.run(
        ["systemctl", "--user", "show-environment"], check=True, stdout=subprocess.DEVNULL
    )
    unit_dir = (
        Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    )
    unit_dir.mkdir(parents=True, exist_ok=True)
    # Validate ownership before modifying config or services.
    for name in units:
        target = unit_dir / name
        if target.exists() and not target.read_text().startswith(MARKER):
            expected = {
                "litellm-trapi.service": "Description=Local TRAPI LiteLLM gateway (Managed Identity)",
                "litellm-trapi-sync.service": "Description=Discover TRAPI models and update the local LiteLLM gateway",
                "litellm-trapi-sync.timer": "Description=Refresh TRAPI model catalog every hour",
            }[name]
            if expected not in target.read_text():
                raise ValueError(f"Refusing to replace unrelated unit: {target}")
    preflight_units(units)
    result = sync_models.sync(bootstrap=True, no_reload=True)
    gateway_changed = False
    for name, content in units.items():
        changed = managed_write(
            unit_dir / name, content, legacy=content.split("Description=")[1].split("\n")[0]
        )
        gateway_changed |= changed and name == settings.SERVICE
    write_client_files(settings.CONFIG_DIR, settings.PORT)
    subprocess.run(
        ["systemd-analyze", "--user", "verify", *[str(unit_dir / name) for name in units]],
        check=True,
    )
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    if args.enable_linger:
        subprocess.run(["loginctl", "enable-linger", str(os.getuid())], check=True)
    subprocess.run(
        ["systemctl", "--user", "enable", "--now", settings.SERVICE, "litellm-trapi-sync.timer"],
        check=True,
    )
    if gateway_changed:
        # Executable or unit changes need a restart; catalog-only changes use HUP.
        subprocess.run(["systemctl", "--user", "restart", settings.SERVICE], check=True)
    else:
        # Also load updated Python source on an idempotent redeploy.
        sync_models.reload_service()
    config = sync_models.yaml.safe_load(settings.CONFIG_PATH.read_text())
    sync_models.wait_for_models(
        {item["model_name"] for item in config["model_list"]}, result["config_sha256"]
    )
    print(
        json.dumps(
            {
                "status": "ready",
                "base_url": settings.LOCAL_URL + "/v1",
                "models": len(config["model_list"]),
                "key_file": str(settings.KEY_PATH),
            }
        )
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
