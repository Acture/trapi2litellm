"""Offline checks of installation boundaries and the foreground server command."""

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

from trapi2litellm import cli, deploy, server


class CliTests(unittest.TestCase):
    def test_help_version_and_subcommands_without_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            for args in (("--help",), ("--version",), ("deploy", "--help"), ("serve", "--help")):
                result = subprocess.run(
                    [sys.executable, "-m", "trapi2litellm", *args],
                    cwd=folder,
                    env={
                        **os.environ,
                        "TRAPI_BASE_URL": "invalid",
                        "TRAPI2LITELLM_PORT": "invalid",
                    },
                    check=True,
                    capture_output=True,
                    text=True,
                )
                self.assertIn("trapi2litellm", result.stdout)

    def test_unknown_options_fail(self) -> None:
        with self.assertRaises(SystemExit) as failure:
            cli.run(["serve", "--unknown"])
        self.assertEqual(failure.exception.code, 2)

    def test_dry_run_never_invokes_subprocesses(self) -> None:
        with patch.object(subprocess, "run", side_effect=AssertionError("subprocess in dry-run")):
            with redirect_stdout(io.StringIO()) as output:
                self.assertEqual(
                    deploy.main(["--dry-run", "--entry-point", "/usr/bin/trapi2litellm"]), 0
                )
            self.assertIn('ExecStart="/usr/bin/trapi2litellm" serve', output.getvalue())

    def test_linger_requires_explicit_start(self) -> None:
        with self.assertRaises(SystemExit):
            deploy.main(["--enable-linger"])

    def test_install_only_does_not_sync_enable_or_start(self) -> None:
        from trapi2litellm import settings, sync_models

        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            with (
                patch.object(settings, "CONFIG_DIR", root / "config"),
                patch.object(settings, "STATE_DIR", root / "state"),
                patch.dict(os.environ, {"XDG_CONFIG_HOME": str(root / "xdg")}),
                patch.object(deploy.sys, "platform", "linux"),
                patch.object(deploy, "entry_point", return_value=Path("/usr/bin/python3")),
                patch.object(deploy, "preflight_units"),
                patch.object(subprocess, "run") as invoke,
                patch.object(
                    sync_models,
                    "sync",
                    side_effect=AssertionError("authentication in install-only deploy"),
                ),
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(deploy.main([]), 0)
                self.assertFalse((root / "config/gateway.env").exists())
                for call in invoke.call_args_list:
                    self.assertFalse(
                        {"enable", "start", "restart", "loginctl"}.intersection(call.args[0])
                    )

    def test_uvx_cache_and_custom_cache_are_refused(self) -> None:
        for entry in (
            Path(tempfile.gettempdir()) / "uv/bin/trapi2litellm",
            Path.home() / ".cache/uv/archive/bin/trapi2litellm",
        ):
            self.assertIsNotNone(deploy.persistence_problem(entry))
        with patch.dict(os.environ, {"UV_CACHE_DIR": "/opt/custom-uv"}):
            self.assertIsNotNone(
                deploy.persistence_problem(Path("/opt/custom-uv/archive/bin/trapi2litellm"))
            )

    def test_stable_symlinks_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            entry = Path(folder) / "trapi2litellm"
            entry.symlink_to("/opt/versioned/bin/trapi2litellm")
            self.assertEqual(deploy.entry_point(entry), entry)

    def test_editable_and_cache_tags_refused(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            environment = root / "environment"
            environment.mkdir()
            (environment / "pyvenv.cfg").touch()
            record = (
                environment
                / "lib/python3.12/site-packages/trapi2litellm-0.1.0.dist-info/direct_url.json"
            )
            record.parent.mkdir(parents=True)
            record.write_text(json.dumps({"dir_info": {"editable": True}}))
            # Only for inspecting tag/editable handling: remove the known /tmp
            # root from this check so that it cannot mask the specific reason.
            with patch.object(Path, "is_relative_to", return_value=False):
                self.assertIn(
                    "editable", deploy.persistence_problem(environment / "bin/trapi2litellm") or ""
                )
                record.unlink()
                (root / "CACHEDIR.TAG").touch()
                self.assertIn(
                    "cache", deploy.persistence_problem(environment / "bin/trapi2litellm") or ""
                )

    def test_installed_system_command_is_persistent(self) -> None:
        self.assertIsNone(deploy.persistence_problem(Path("/usr/bin/trapi2litellm")))

    def test_serve_uses_current_interpreter_and_loopback(self) -> None:
        with patch.dict(os.environ, {"LITELLM_MASTER_KEY": "offline-test-key"}):
            with patch.object(os, "execv", side_effect=SystemExit(0)) as execute:
                with self.assertRaises(SystemExit):
                    server.main([])
        executable, command = execute.call_args.args
        self.assertEqual(executable, sys.executable)
        self.assertEqual(
            command[:4], [sys.executable, "-m", "gunicorn", "trapi2litellm.gateway_app:app"]
        )
        self.assertIn("127.0.0.1:4000", command)
        self.assertIn("uvicorn_worker.UvicornWorker", command)


if __name__ == "__main__":
    unittest.main()
