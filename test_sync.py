"""Offline regression checks; no credential or inference requests."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import sync_models as sync


def catalog(*names):
    return {
        "data": [
            {
                "id": name,
                "provisioningState": "Succeeded",
                "capabilities": {"chatCompletion": "true"},
            }
            for name in names
        ]
    }


class ConfigTests(unittest.TestCase):
    def test_deterministic_order(self):
        self.assertEqual(sync.build_config(catalog("b", "a")), sync.build_config(catalog("a", "b")))

    def test_no_generated_timestamp(self):
        self.assertNotIn("fetched_at", str(sync.build_config(catalog("a"))))

    def test_reject_empty(self):
        with self.assertRaises(ValueError):
            sync.build_config({"data": []})

    def test_reject_pagination(self):
        data = catalog("a")
        data["nextLink"] = "https://example.invalid/page2"
        with self.assertRaises(ValueError):
            sync.build_config(data)

    def test_reject_invalid_model_metadata(self):
        data = catalog("a")
        data["data"][0]["model"] = "not-an-object"
        with self.assertRaises(ValueError):
            sync.build_config(data)

    def test_reject_duplicates(self):
        with self.assertRaises(ValueError):
            sync.build_config(catalog("a", "a"))

    def test_reject_unsafe_names(self):
        for name in ["../x", "a/../b", "https://bad.invalid", "a\nfoo", "a?key=x"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                sync.build_config(catalog(name))

    def test_slash_deployment(self):
        entry = sync.build_config(catalog("Qwen/Qwen3.5-9B"))["model_list"][0]
        self.assertEqual(entry["litellm_params"]["model"], "azure/Qwen/Qwen3.5-9B")

    def test_failed_provisioning_excluded(self):
        data = catalog("a", "b")
        data["data"][1]["provisioningState"] = "Failed"
        self.assertEqual(len(sync.build_config(data)["model_list"]), 1)

    def test_responses_mode_not_chat(self):
        data = catalog("codex")
        data["data"][0]["capabilities"] = {"responses": "true"}
        entry = sync.build_config(data)["model_list"][0]
        self.assertEqual(entry["model_info"]["mode"], "responses")

    def test_missing_capabilities_not_invented(self):
        data = catalog("unknown")
        data["data"][0]["capabilities"] = {}
        self.assertNotIn("mode", sync.build_config(data)["model_list"][0]["model_info"])

    def test_bool_capabilities(self):
        self.assertTrue(sync.enabled({"responses": True}, "responses"))
        self.assertFalse(sync.enabled({"responses": "false"}, "responses"))

    def test_base_model_does_not_rewrite_deployment(self):
        data = catalog("gpt-5.2_2025-12-11")
        data["data"][0]["model"] = {"Format": "OpenAI", "Name": "gpt-5.2"}
        entry = sync.build_config(data)["model_list"][0]
        self.assertEqual(entry["model_info"]["base_model"], "azure/gpt-5.2")
        self.assertEqual(entry["litellm_params"]["model"], "azure/gpt-5.2_2025-12-11")

    def test_raw_capabilities_retained(self):
        data = catalog("a")
        data["data"][0]["capabilities"]["maxContextToken"] = "1234"
        entry = sync.build_config(data)["model_list"][0]
        self.assertEqual(entry["model_info"]["capabilities"], data["data"][0]["capabilities"])

    def test_key_not_embedded_and_params_not_dropped(self):
        config = sync.build_config(catalog("a"))
        self.assertEqual(config["general_settings"]["master_key"], "os.environ/LITELLM_MASTER_KEY")
        self.assertFalse(config["litellm_settings"]["drop_params"])
        self.assertEqual(config["router_settings"]["fallbacks"], [])

    def test_schema_valid(self):
        sync.validate_config(sync.build_config(catalog("a")), None)

    def test_mass_removal_rejected(self):
        old = sync.build_config(catalog("a", "b", "c", "d"))
        with self.assertRaises(ValueError):
            sync.validate_config(sync.build_config(catalog("a")), old)


class PublishTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        for name, value in {
            "CONFIG_DIR": root / "config",
            "STATE_DIR": root / "state",
            "CONFIG_PATH": root / "config" / "config.yaml",
            "KEY_PATH": root / "config" / "gateway.env",
        }.items():
            patcher = patch.object(sync, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_bootstrap_preserves_existing_key(self):
        sync.setup_directories()
        sync.bootstrap_key()
        key = sync.local_key()
        sync.bootstrap_key()
        self.assertEqual(sync.local_key(), key)
        self.assertEqual(sync.KEY_PATH.stat().st_mode & 0o777, 0o600)

    @patch.object(sync, "fetch_catalog", return_value=catalog("a", "b"))
    @patch.object(sync, "reload_service")
    @patch.object(sync, "service_active", return_value=False)
    def test_second_sync_is_noop(self, active, reload, fetch):
        self.assertTrue(sync.sync(bootstrap=True)["changed"])
        self.assertFalse(sync.sync()["changed"])
        reload.assert_not_called()

    @patch.object(sync, "fetch_catalog", return_value=catalog("a", "b"))
    @patch.object(sync, "service_active", return_value=False)
    def test_fetch_error_keeps_previous(self, active, fetch):
        sync.sync(bootstrap=True)
        before = sync.CONFIG_PATH.read_bytes()
        fetch.side_effect = RuntimeError("offline")
        with self.assertRaises(RuntimeError):
            sync.sync()
        self.assertEqual(sync.CONFIG_PATH.read_bytes(), before)

    @patch.object(sync, "fetch_catalog", return_value=catalog("a", "b"))
    @patch.object(sync, "service_active", return_value=False)
    def test_failed_reload_rolls_back(self, active, fetch):
        sync.sync(bootstrap=True)
        before = sync.CONFIG_PATH.read_bytes()
        active.return_value = True
        fetch.return_value = catalog("a", "b", "c")
        with (
            patch.object(sync, "reload_service") as reload,
            patch.object(
                sync,
                "wait_for_models",
                side_effect=RuntimeError("not ready"),
            ),
        ):
            with self.assertRaises(RuntimeError):
                sync.sync()
        self.assertEqual(reload.call_count, 2)
        self.assertEqual(sync.CONFIG_PATH.read_bytes(), before)
        self.assertTrue((sync.STATE_DIR / "config.rejected.yaml").exists())


if __name__ == "__main__":
    unittest.main()
