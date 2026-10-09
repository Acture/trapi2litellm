"""Offline Debian install/upgrade/remove/purge acceptance in a disposable container."""

import argparse
import hashlib
import logging
import os
import subprocess
import sys
from pathlib import Path

from offline_gateway import prepare, probe, seed_usage, wait_ready

LOG = logging.getLogger(__name__)


def run(command: list[str], *, user: bool = False) -> str:
    if user:
        uid = subprocess.check_output(["id", "-u", "acceptance"], text=True).strip()
        command = [
            "runuser",
            "-u",
            "acceptance",
            "--",
            "env",
            f"XDG_RUNTIME_DIR=/run/user/{uid}",
            "LITELLM_LOCAL_MODEL_COST_MAP=True",
            *command,
        ]
    LOG.info("Running %s", " ".join(command))
    try:
        result = subprocess.run(command, check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as error:
        LOG.error("%s\n%s", error.stdout, error.stderr)
        raise
    return result.stdout


def assert_installed(config: Path, state: Path) -> None:
    version = run(["trapi2litellm", "--version"], user=True)
    LOG.info(version.strip())
    preview = run(["trapi2litellm", "deploy", "--dry-run"], user=True)
    if 'ExecStart="/usr/bin/trapi2litellm" serve' not in preview or "WorkingDirectory" in preview:
        raise AssertionError("Units did not use the stable package entry point")
    run(["trapi2litellm", "serve", "--help"], user=True)
    run(
        [
            "/opt/trapi2litellm/bin/python",
            "-c",
            "import azure.identity, litellm.types.router, uvicorn_worker; from trapi2litellm.runtime import validate; validate({'config': {'model_list': [{'model_name': 'trapi/offline', 'litellm_params': {'model': 'azure/offline'}}]}})",
        ],
        user=True,
    )
    if any(config.iterdir()) or any(state.iterdir()):
        raise AssertionError("Installation or preview wrote user configuration/state")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("old", type=Path)
    parser.add_argument("new", type=Path)
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    if not sys.platform.startswith("linux") or os.getuid() != 0:
        raise ValueError("Run only as root in a disposable Debian/Ubuntu test container")
    # Docker acceptance is invoked with --network none. Refuse an accidentally
    # connected container rather than mistaking an online install for offline.
    if any(path.name != "lo" for path in Path("/sys/class/net").iterdir()):
        raise ValueError("Run the acceptance container with --network none")
    run(["useradd", "--create-home", "acceptance"])
    home = Path("/home/acceptance")
    config = home / ".config/litellm-trapi"
    state = home / ".local/state/trapi2litellm"
    for path in (config, state):
        path.mkdir(parents=True)
        path.chmod(0o700)
    run(["chown", "-R", "acceptance:acceptance", str(home)])
    run(["dpkg", "--install", str(args.old.resolve())])
    assert_installed(config, state)
    uid = run(["id", "-u", "acceptance"]).strip()
    run(["systemctl", "start", f"user@{uid}.service"])
    run(["trapi2litellm", "deploy"], user=True)
    inactive = subprocess.run(
        [
            "runuser",
            "-u",
            "acceptance",
            "--",
            "env",
            f"XDG_RUNTIME_DIR=/run/user/{uid}",
            "systemctl",
            "--user",
            "is-active",
            "--quiet",
            "litellm-trapi.service",
        ],
        check=False,
    )
    if inactive.returncode == 0:
        raise AssertionError("Install-only deployment started the service")
    prepare(config, state)
    run(["chown", "-R", "acceptance:acceptance", str(home)])
    run(["systemctl", "--user", "start", "litellm-trapi.service"], user=True)
    wait_ready(4000)
    seed_usage(state)
    probe(4000)
    digest = hashlib.sha256((config / "gateway.env").read_bytes()).hexdigest()
    run(["dpkg", "--install", str(args.new.resolve())])
    if hashlib.sha256((config / "gateway.env").read_bytes()).hexdigest() != digest:
        raise AssertionError("Upgrade changed the local gateway key")
    run(["systemctl", "--user", "restart", "litellm-trapi.service"], user=True)
    wait_ready(4000)
    probe(4000)
    run(
        ["systemctl", "--user", "stop", "litellm-trapi.service", "litellm-trapi-sync.timer"],
        user=True,
    )
    # Use a non-secret fixture to prove package removal leaves user's files.
    key = config / "gateway.env"
    key.write_text("LITELLM_MASTER_KEY=offline-acceptance-key\n")
    key.chmod(0o600)
    (state / "retained.json").write_text('{"retained": true}\n')
    digest = hashlib.sha256(key.read_bytes()).hexdigest()
    for operation in ("--remove", "--purge"):
        run(["dpkg", operation, "trapi2litellm"])
        if (
            Path("/usr/bin/trapi2litellm").exists()
            or Path("/usr/bin/trapi2litellm").is_symlink()
            or Path("/opt/trapi2litellm").exists()
        ):
            raise AssertionError("Uninstall left package files behind")
        if (
            not (state / "retained.json").is_file()
            or hashlib.sha256(key.read_bytes()).hexdigest() != digest
        ):
            raise AssertionError("Uninstall changed user state or local key")
        run(["dpkg", "--install", str(args.new.resolve())])
        run(["trapi2litellm", "--version"], user=True)
    LOG.info("Accepted offline install, upgrade, remove, purge and reinstall")


if __name__ == "__main__":
    main()
