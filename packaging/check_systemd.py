"""Accept install-only deployment and the generated persistent user service offline."""

import hashlib
import os
import subprocess
from pathlib import Path
from urllib.error import URLError

from offline_gateway import prepare, probe, wait_ready


def run(command: list[str]) -> None:
    subprocess.run(command, check=True)


def main() -> None:
    # check_dist supplies explicit temporary config/state, a persistent uv tool
    # root and the real runner's systemd user manager. Do not call deploy --start:
    # that would intentionally authenticate with Azure and fetch the catalog.
    config = Path(os.environ["TRAPI2LITELLM_CONFIG_DIR"])
    state = Path(os.environ["TRAPI2LITELLM_STATE_DIR"])
    unit_dir = (
        Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user"
    )
    names = ("litellm-trapi.service", "litellm-trapi-sync.service", "litellm-trapi-sync.timer")
    prepare(config, state)
    try:
        run(["systemctl", "--user", "start", names[0]])
        wait_ready(4000)
        probe(4000)
        with (config / "config.yaml").open("a") as handle:
            handle.write("# Offline systemd reload acceptance\n")
        digest = hashlib.sha256((config / "config.yaml").read_bytes()).hexdigest()
        run(["systemctl", "--user", "reload", names[0]])
        wait_ready(4000, digest=digest)
        probe(4000)
    except (RuntimeError, URLError, TimeoutError, subprocess.CalledProcessError):
        subprocess.run(
            ["journalctl", "--user", "-u", names[0], "--no-pager", "-n", "50"], check=False
        )
        raise
    finally:
        run(["systemctl", "--user", "stop", names[0]])
        for name in names:
            path = unit_dir / name
            if path.exists() and path.read_text().startswith("# Managed by trapi2litellm\n"):
                path.unlink()
        run(["systemctl", "--user", "daemon-reload"])
    print("Accepted persistent uv tool service startup/reload with an offline fixture")


if __name__ == "__main__":
    main()
