"""Deployment rendering checks. Never touches live units or credentials."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import deploy


class DeploymentTests(unittest.TestCase):
    def test_render_one_listener_and_no_secrets(self):
        units = deploy.render_units(
            Path("/opt/gateway"),
            Path("/private/config"),
            Path("/private/state"),
            4567,
            {"TRAPI_BASE_URL": "https://example.invalid/pool"},
        )
        self.assertEqual(len(units), 3)
        service = units["litellm-trapi.service"]
        self.assertIn("--bind 127.0.0.1:4567", service)
        self.assertIn("EnvironmentFile=/private/config/gateway.env", service)
        self.assertNotIn("LITELLM_MASTER_KEY=", service)
        self.assertIn("AZURE_TOKEN_CREDENTIALS=ManagedIdentityCredential", service)
        self.assertIn("ExecReload=/bin/kill -HUP $MAINPID", service)

    def test_unit_paths_escape_specifiers(self):
        self.assertEqual(deploy.unit_quote("/a b/%u"), '"/a b/%%u"')
        with self.assertRaises(ValueError):
            deploy.unit_quote("/tmp/a\nExecStart=oops")

    def test_refuse_foreign_unit(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "example.service"
            path.write_text("[Unit]\nDescription=Some other service\n")
            with self.assertRaises(ValueError):
                deploy.managed_write(path, deploy.MARKER + "[Unit]\n")
            self.assertIn("Some other service", path.read_text())

    def test_noop_and_backup(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "example.service"
            original = deploy.MARKER + "first\n"
            self.assertTrue(deploy.managed_write(path, original))
            self.assertFalse(deploy.managed_write(path, original))
            self.assertTrue(deploy.managed_write(path, deploy.MARKER + "second\n"))
            self.assertEqual(path.with_suffix(".service.previous").read_text(), original)

    def test_clients_reference_key_file(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder)
            deploy.write_client_files(path, 4001)
            for name in ["client.sh", "client.fish"]:
                text = (path / name).read_text()
                self.assertIn("gateway.env", text)
                self.assertIn("127.0.0.1:4001/v1", text)
                self.assertNotIn("sk-trapi-", text)

    def test_dry_run_has_no_side_effects(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            result = subprocess.run(
                [
                    sys.executable,
                    str(deploy.SOURCE / "deploy.py"),
                    "--dry-run",
                    "--config-dir",
                    str(root / "config"),
                    "--state-dir",
                    str(root / "state"),
                    "--port",
                    "4567",
                ],
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertIn("127.0.0.1:4567", result.stdout)
            self.assertFalse((root / "config").exists())
            self.assertFalse((root / "state").exists())

    def test_settings_override(self):
        with tempfile.TemporaryDirectory() as folder:
            result = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "import settings; print(settings.CONFIG_DIR); print(settings.LOCAL_URL)",
                ],
                env={
                    **os.environ,
                    "TRAPI2LITELLM_CONFIG_DIR": folder,
                    "TRAPI2LITELLM_PORT": "4567",
                },
                cwd=deploy.SOURCE,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertIn(folder, result.stdout)
            self.assertIn("127.0.0.1:4567", result.stdout)


if __name__ == "__main__":
    unittest.main()
