"""Build a disposable source copy, delete it, then accept wheel/sdist and uv tools."""

import argparse
import logging
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

LOG = logging.getLogger(__name__)


def run(command: list[str], *, cwd: Path, env: dict[str, str], capture: bool = False) -> str:
    started = time.monotonic()
    LOG.info("Running %s", " ".join(command))
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )
    LOG.info("Finished in %.1fs", time.monotonic() - started)
    return result.stdout or ""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--out", type=Path, default=Path("dist"))
    parser.add_argument(
        "--tool-root",
        type=Path,
        help="Persistent directory outside source/cache for uv tool acceptance",
    )
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    root = Path(__file__).resolve().parents[1]
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="trapi2litellm-dist-") as folder:
        work = Path(folder)
        source = work / "source"
        shutil.copytree(
            root,
            source,
            ignore=shutil.ignore_patterns(
                ".git",
                ".venv",
                "notes",
                ".superpowers",
                "target",
                "dist",
                "debs",
                "debs-old",
                "__pycache__",
                ".ruff_cache",
            ),
        )
        env = {
            **os.environ,
            "LITELLM_LOCAL_MODEL_COST_MAP": "True",
            "TRAPI2LITELLM_CONFIG_DIR": str(work / "config"),
            "TRAPI2LITELLM_STATE_DIR": str(work / "state"),
            "UV_TOOL_DIR": str(work / "uvx-tools"),
            "UV_TOOL_BIN_DIR": str(work / "uvx-bin"),
        }
        run(["uv", "build", "--python", args.python, "--out-dir", str(output)], cwd=source, env=env)
        requirements = output / "requirements.txt"
        run(
            ["uv", "export", "--frozen", "--no-dev", "--no-emit-project", "-o", str(requirements)],
            cwd=source,
            env=env,
            capture=True,
        )
        (wheel,) = output.glob("trapi2litellm-*.whl")
        (sdist,) = output.glob("trapi2litellm-*.tar.gz")
        with tarfile.open(sdist) as archive:
            archive.extractall(work / "sdist", filter="data")
        (unpacked,) = (work / "sdist").iterdir()
        tests = work / "tests"
        shutil.copytree(unpacked / "tests", tests)
        shutil.rmtree(unpacked)
        shutil.rmtree(source)
        for kind, artifact in (("wheel", wheel), ("sdist", sdist)):
            venv = work / kind
            run(["uv", "venv", "--python", args.python, str(venv)], cwd=work, env=env)
            python = str(venv / "bin/python")
            run(
                ["uv", "pip", "sync", "--python", python, "--require-hashes", str(requirements)],
                cwd=work,
                env=env,
            )
            run(
                ["uv", "pip", "install", "--python", python, "--no-deps", str(artifact)],
                cwd=work,
                env=env,
            )
            command = str(venv / "bin/trapi2litellm")
            for options in (
                ["--version"],
                ["--help"],
                ["deploy", "--dry-run"],
                ["serve", "--help"],
                ["sync", "--help"],
            ):
                run([command, *options], cwd=work, env=env, capture=True)
            run([python, "-m", "unittest", "discover", "-s", str(tests), "-v"], cwd=work, env=env)
            run(
                [args.python, str(root / "packaging/offline_gateway.py"), command],
                cwd=work,
                env=env,
            )
        uvx = [
            "uv",
            "tool",
            "run",
            "--python",
            args.python,
            "--from",
            str(wheel),
            "--with-requirements",
            str(requirements),
            "trapi2litellm",
        ]
        run([*uvx, "--version"], cwd=work, env=env)
        run([*uvx, "deploy", "--dry-run"], cwd=work, env=env, capture=True)
        rejected = subprocess.run(
            [*uvx, "deploy"], cwd=work, env=env, capture_output=True, text=True
        )
        if rejected.returncode == 0 or "non-persistent" not in rejected.stderr:
            raise AssertionError(f"uvx deployment was not refused: {rejected.stderr}")
        tool_root = args.tool_root.resolve() if args.tool_root else work / "tools"
        tool_env = {
            **env,
            "UV_TOOL_DIR": str(tool_root / "envs"),
            "UV_TOOL_BIN_DIR": str(tool_root / "bin"),
        }
        if args.tool_root:
            # PrivateTmp deliberately hides /tmp from the generated service.
            # Exercise systemd's raw EnvironmentFile path parsing, including
            # whitespace, specifiers and characters that quoted values escape.
            tool_env["TRAPI2LITELLM_CONFIG_DIR"] = str(
                tool_root / "runtime" / 'config space%u"\\$HOME'
            )
            tool_env["TRAPI2LITELLM_STATE_DIR"] = str(tool_root / "runtime/state")
        run(
            [
                "uv",
                "tool",
                "install",
                "--force",
                "--python",
                args.python,
                "--with-requirements",
                str(requirements),
                str(wheel),
            ],
            cwd=work,
            env=tool_env,
        )
        command = str(tool_root / "bin/trapi2litellm")
        tool_env["PATH"] = str(tool_root / "bin") + os.pathsep + tool_env["PATH"]
        run([command, "--version"], cwd=work, env=tool_env)
        preview = run([command, "deploy", "--dry-run"], cwd=work, env=tool_env, capture=True)
        if f'ExecStart="{command}" serve' not in preview or "WorkingDirectory" in preview:
            raise AssertionError("uv tool unit did not use its stable command")
        if args.tool_root:
            rejected = subprocess.run(
                [command, "deploy"], cwd=work, env=tool_env, capture_output=True, text=True
            )
            # A non-Linux host can reject systemd; a persistent Linux CI host
            # must complete install-only deployment without Azure access.
            if sys.platform.startswith("linux"):
                if rejected.returncode:
                    print(rejected.stderr, file=sys.stderr)
                rejected.check_returncode()
                run([args.python, str(root / "packaging/check_systemd.py")], cwd=work, env=tool_env)
            elif "non-persistent" in rejected.stderr:
                raise AssertionError(rejected.stderr)
        LOG.info("Accepted installed wheel/sdist, uvx refusal and uv tool entry")


if __name__ == "__main__":
    main()
