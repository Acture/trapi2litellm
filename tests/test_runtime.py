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

    def test_native_config_path_survives_runtime_directory_change(self) -> None:
        command: Path = Path(
            os.environ.get(
                "TRAPI2LITELLM_TEST_NATIVE", str(Path(sys.executable).parent / "trapi2litellm")
            )
        )
        with tempfile.TemporaryDirectory() as folder:
            root: Path = Path(folder).resolve()
            fixture: Path = root / "trapi2litellm"
            fixture.mkdir()
            (fixture / "__init__.py").write_text("")
            # Run the real runtime after native exec; observe only its terminal
            # Gunicorn exec, after serve has actually changed the process cwd.
            (fixture / "runtime.py").write_text(
                "import importlib.util, json, os\n"
                "from pathlib import Path\n"
                "from unittest.mock import patch\n"
                f"spec = importlib.util.spec_from_file_location('real_runtime', {runtime.__file__!r})\n"
                "module = importlib.util.module_from_spec(spec)\n"
                "spec.loader.exec_module(module)\n"
                "def observe_exec(executable: str, arguments: list[str]) -> None:\n"
                "    path: str = os.environ['CONFIG_FILE_PATH']\n"
                "    print(json.dumps({'path': path, 'content': Path(path).read_text(),\n"
                "                      'cwd': os.getcwd(), 'pid': os.getpid(),\n"
                "                      'arguments': arguments}))\n"
                "with patch.object(os, 'execv', side_effect=observe_exec):\n"
                "    module.serve()\n"
            )
            config: Path = root / "config"
            config.mkdir()
            (config / "config.yaml").write_text("default configuration\n")
            (root / "custom.yaml").write_text("explicit configuration\n")
            absolute: str = f"{root}/../{root.name}/custom.yaml"
            for override, expected_path, content in (
                (None, str(config / "config.yaml"), "default configuration\n"),
                ("custom.yaml", str(root / "custom.yaml"), "explicit configuration\n"),
                (absolute, absolute, "explicit configuration\n"),
            ):
                env: dict[str, str] = {
                    **{
                        key: value for key, value in os.environ.items() if key != "CONFIG_FILE_PATH"
                    },
                    "TRAPI2LITELLM_PYTHON": sys.executable,
                    "TRAPI2LITELLM_CONFIG_DIR": str(config),
                    "TRAPI_BASE_URL": "https://example.invalid/pool",
                    "LITELLM_MASTER_KEY": "sk-exec-regression",
                    "PYTHONPATH": str(root),
                }
                if override is not None:
                    env["CONFIG_FILE_PATH"] = override
                with (
                    self.subTest(override=override),
                    subprocess.Popen(
                        [str(command), "serve", "--port", "4567"],
                        cwd=root,
                        env=env,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                    ) as process,
                ):
                    stdout, stderr = process.communicate(timeout=15)
                    self.assertEqual(process.returncode, 0, stderr)
                    result: dict[str, object] = json.loads(stdout)
                    self.assertEqual(result["path"], expected_path)
                    self.assertEqual(result["content"], content)
                    self.assertEqual(result["pid"], process.pid)
                    self.assertEqual(
                        result["cwd"], str(Path(runtime.__file__).resolve().parent.parent)
                    )
                    arguments: object = result["arguments"]
                    if not isinstance(arguments, list):
                        self.fail("Expected Gunicorn argument list")
                    self.assertIn("127.0.0.1:4567", arguments)

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
