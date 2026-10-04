"""Offline checks of the SDK/schema boundary and native installed command."""

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import httpx

from trapi2litellm import runtime

os.environ.setdefault("LITELLM_LOCAL_MODEL_COST_MAP", "True")

CONFIG: dict[str, object] = {
    "model_list": [
        {
            "model_name": "trapi/new",
            "litellm_params": {
                "model": "azure/new",
                "api_base": "https://example.invalid/pool",
                "api_version": "2025-04-01-preview",
                "azure_scope": "api://trapi/.default",
            },
        }
    ]
}


class RuntimeTests(unittest.TestCase):
    def test_previous_yaml_is_parsed_at_schema_boundary(self) -> None:
        result = runtime.validate(
            {
                "config": CONFIG,
                "previous_text": "model_list:\n- model_name: trapi/old\n",
            }
        )
        self.assertEqual(result, {"previous_models": ["trapi/old"]})

    def test_schema_rejects_missing_provider_model(self) -> None:
        with self.assertRaises((ValueError, TypeError)):
            runtime.validate({"config": {"model_list": [{"model_name": "trapi/bad"}]}})

    def test_malformed_previous_config_fails_closed(self) -> None:
        for text in ("[]", "model_list: invalid", "model_list: [{}]"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                runtime.validate({"config": CONFIG, "previous_text": text})

    def test_json_subprocess_validates_without_credentials(self) -> None:
        result = subprocess.run(
            [sys.executable, "-m", "trapi2litellm.runtime", "validate"],
            input=json.dumps({"config": CONFIG, "previous_text": None}),
            text=True,
            capture_output=True,
            check=True,
        )
        self.assertEqual(json.loads(result.stdout), {"previous_models": []})

    def test_sdk_failure_does_not_expose_details(self) -> None:
        with (
            patch.object(sys, "argv", ["runtime", "catalog"]),
            patch.object(sys, "stdin", io.StringIO("{}")),
            patch.object(runtime, "catalog", side_effect=RuntimeError("Bearer secret-token")),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(runtime.main(), 1)
        self.assertEqual(json.loads(output.getvalue()), {"error_type": "RuntimeError"})
        self.assertNotIn("secret-token", output.getvalue())

    def test_http_error_exposes_status_only(self) -> None:
        response = httpx.Response(403, request=httpx.Request("GET", "https://secret.invalid"))
        with (
            patch.object(sys, "argv", ["runtime", "catalog"]),
            patch.object(sys, "stdin", io.StringIO("{}")),
            patch.object(
                runtime,
                "catalog",
                side_effect=httpx.HTTPStatusError(
                    "sensitive detail", request=response.request, response=response
                ),
            ),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(runtime.main(), 1)
        self.assertEqual(
            json.loads(output.getvalue()), {"error_type": "HTTPStatusError", "http_status": 403}
        )

    def test_catalog_uses_identity_scope_and_preserves_base_path(self) -> None:
        credential = MagicMock()
        credential.__enter__.return_value = credential
        credential.get_token.return_value.token = "sdk-token"
        response = MagicMock()
        response.json.return_value = {"data": []}
        client = MagicMock()
        client.__enter__.return_value = client
        client.get.return_value = response
        with (
            patch("azure.identity.ManagedIdentityCredential", return_value=credential) as identity,
            patch.object(httpx, "Client", return_value=client) as transport,
        ):
            self.assertEqual(
                runtime.catalog(
                    {
                        "base_url": "https://trapi.invalid/redmond/interactive",
                        "catalog_version": "preview",
                        "scope": "api://trapi/.default",
                        "client_id": "identity-selector",
                    }
                ),
                {"data": []},
            )
        identity.assert_called_once_with(client_id="identity-selector")
        credential.get_token.assert_called_once_with("api://trapi/.default")
        transport.assert_called_once_with(timeout=30, follow_redirects=False, trust_env=False)
        client.get.assert_called_once_with(
            "https://trapi.invalid/redmond/interactive/openai/models?api-version=preview",
            headers={"Authorization": "Bearer sdk-token"},
        )

    def test_serve_exec_preserves_current_python_and_loopback(self) -> None:
        with (
            patch.dict(os.environ, {"TRAPI2LITELLM_PORT": "4567"}),
            patch.object(os, "chdir") as change_directory,
            patch.object(os, "execv", side_effect=SystemExit(0)) as execute,
            self.assertRaises(SystemExit),
        ):
            runtime.serve()
        executable, command = execute.call_args.args
        self.assertEqual(executable, sys.executable)
        self.assertEqual(
            command[:4], [sys.executable, "-m", "gunicorn", "trapi2litellm.gateway_app:app"]
        )
        self.assertIn("127.0.0.1:4567", command)
        self.assertIn("900", command)
        change_directory.assert_called_once_with(Path(runtime.__file__).resolve().parent.parent)
        self.assertEqual(
            command[command.index("--chdir") + 1],
            str(Path(runtime.__file__).resolve().parent.parent),
        )

    def test_installed_native_help_is_independent_of_settings(self) -> None:
        command = Path(sys.executable).parent / "trapi2litellm"
        self.assertIn(
            command.read_bytes()[:4],
            (b"\x7fELF", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe"),
        )
        with tempfile.TemporaryDirectory() as folder:
            for args in (("--help",), ("--version",), ("deploy", "--help"), ("serve", "--help")):
                result = subprocess.run(
                    [str(command), *args],
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

    def test_serve_keeps_isolated_interpreter_mode(self) -> None:
        with (
            patch.dict(os.environ, {"TRAPI2LITELLM_PORT": "4567"}),
            patch.object(sys, "flags", SimpleNamespace(isolated=1)),
            patch.object(os, "chdir"),
            patch.object(os, "execv", side_effect=SystemExit(0)) as execute,
            self.assertRaises(SystemExit),
        ):
            runtime.serve()
        self.assertEqual(execute.call_args.args[1][:4], [sys.executable, "-I", "-m", "gunicorn"])

    def test_native_dry_run_does_not_create_runtime_files(self) -> None:
        command = Path(sys.executable).parent / "trapi2litellm"
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            result = subprocess.run(
                [
                    str(command),
                    "deploy",
                    "--dry-run",
                    "--config-dir",
                    str(root / "config"),
                    "--state-dir",
                    str(root / "state"),
                    "--port",
                    "4567",
                ],
                env={**os.environ, "TRAPI2LITELLM_PYTHON": "/nonexistent/python"},
                check=True,
                capture_output=True,
                text=True,
            )
            self.assertIn("serve --port 4567", result.stdout)
            self.assertFalse((root / "config").exists())
            self.assertFalse((root / "state").exists())


if __name__ == "__main__":
    unittest.main()
