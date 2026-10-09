"""Running model metadata, rate limits and observed local usage."""

from __future__ import annotations

import hashlib
import json
import math
from datetime import UTC, datetime
from pathlib import Path
from typing import cast

import yaml

from trapi2litellm.usage import RETENTION_SECONDS, WINDOW_SECONDS, UsageStore, empty_usage, mapping


def rate_limits(value: object) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    for name, raw in mapping(value).items():
        field = name.lower()
        kind = (
            "requests"
            if "request" in field or field == "rpm"
            else ("tokens" if "token" in field or field == "tpm" else "other")
        )
        limit = mapping(raw).get("count") if isinstance(raw, dict) else raw
        known = (
            isinstance(limit, (int, float)) and not isinstance(limit, bool) and math.isfinite(limit)
        )
        state = "unknown"
        if known and limit in (0, -1):
            state = "unlimited"
        elif known and cast(int | float, limit) > 0:
            state = "limited"
        result.append(
            {
                "field": name,
                "kind": kind,
                "unit": {"requests": "req/min", "tokens": "tok/min"}.get(kind, name + "/min"),
                "state": state,
                "per_minute": limit if state == "limited" else None,
            }
        )
    return result


class ModelView:
    def __init__(self, config: bytes, state_dir: Path, store: UsageStore) -> None:
        self.config_sha256 = hashlib.sha256(config).hexdigest()
        self.state_dir = state_dir
        self.store = store
        source = mapping(yaml.safe_load(config))
        models = source.get("model_list")
        if not isinstance(models, list):
            raise ValueError("Expected a configured model_list")
        self.models: list[dict[str, object]] = []
        for raw in models:
            entry = mapping(raw)
            name = entry.get("model_name")
            if not isinstance(name, str):
                raise ValueError("Expected a configured model_name")
            info = mapping(entry.get("model_info"))
            self.models.append(
                {
                    "id": name,
                    "upstream_deployment": info.get("upstream_deployment"),
                    "capabilities": info.get("capabilities"),
                    "upstream_model": info.get("upstream_model"),
                    "upstream_rate_limits": info.get("rate_limits"),
                    "limits": rate_limits(info.get("rate_limits")),
                    "availability_evidence": info.get(
                        "availability_evidence", "configured_not_probed"
                    ),
                }
            )
        self.model_names = frozenset(cast(str, model["id"]) for model in self.models)

    def state(self, name: str) -> dict[str, object]:
        path = self.state_dir / (name + ".json")
        return mapping(json.loads(path.read_bytes())) if path.exists() else {}

    def snapshot(self) -> dict[str, object]:
        observed = self.store.snapshot()
        models = [
            {**model, "usage": observed.pop(cast(str, model["id"]), empty_usage())}
            for model in self.models
        ]
        sync = self.state("sync-status")
        return {
            "object": "list",
            "data": models,
            "observed_at": datetime.now(UTC).isoformat(),
            "config_sha256": self.config_sha256,
            "catalog_fetched_at": self.state("catalog").get("fetched_at"),
            "running_config_synced_at": sync.get("checked_at")
            if sync.get("config_sha256") == self.config_sha256
            else None,
            "usage_scope": "requests_through_this_gateway",
            "usage_window_seconds": WINDOW_SECONDS,
            "usage_retention_seconds": RETENTION_SECONDS,
            "estimate_method": "UTF-8 JSON/text bytes divided by four; reported usage takes precedence",
            "token_window": "completed requests in the last minute; in-flight estimates shown separately",
            "rate_limit_enforcement": False,
            "other_usage": [{"model": model, "usage": usage} for model, usage in observed.items()],
        }
