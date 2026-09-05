# trapi2litellm

TRAPI model discovery and a local LiteLLM gateway, using Azure Managed Identity.
One port serves the model APIs, catalog and sync status. No Azure CLI login,
stored upstream bearer token, database, or separate catalog server is needed.

## Deploy on Linux

Prerequisites: an Azure host with a TRAPI-authorized managed identity, a working
`systemctl --user` session, and [uv](https://docs.astral.sh/uv/). Python 3.13 and
dependencies are selected through the checked-in lockfile.

```sh
git clone https://github.com/Acture/trapi2litellm.git
cd trapi2litellm
uv sync --frozen
uv run --frozen python deploy.py --enable-linger
```

The linger option keeps the user service running after logout and starts it at
boot. If your machine requires an administrator for that operation, run
`sudo loginctl enable-linger YOUR_USER` separately, then deploy without the flag.
The deployment command does not install uv, grant TRAPI permissions or configure
firewalls. Re-run it after updating the checkout; existing local keys are kept.

Preview the generated units without writing files or making network calls:

```sh
uv run --frozen python deploy.py --dry-run
```

The source checkout is used by the running service. Do not move or delete it
without redeploying. Unit/executable changes require a service restart; regular
catalog updates replace workers gracefully. The deployer backs up changed units
and refuses to overwrite unrelated services.

## One endpoint

Default client base URL: **http://127.0.0.1:4000/v1**.

```sh
source ~/.config/litellm-trapi/client.sh       # bash/zsh
```

```fish
source ~/.config/litellm-trapi/client.fish     # fish
```

These set `OPENAI_BASE_URL` and `OPENAI_API_KEY` without printing the key.
Every endpoint requires the local gateway key in `Authorization: Bearer ...`.
This key is separate from the auto-refreshed Managed Identity credential.

| Endpoint on port 4000 | Purpose |
| --- | --- |
| `/v1/models` | Configured deployment IDs |
| `/v1/chat/completions` | Chat, streaming and tool calling |
| `/v1/responses` | Responses API |
| `/catalog` | Cached original TRAPI discovery evidence and fetch time |
| `/model/info` | LiteLLM configuration and per-model metadata |
| `/status` | Running configuration hash and sync history |

For example, an OpenAI-compatible SDK uses `trapi/gpt-5.2_2025-12-11` as its
model. A client that itself uses LiteLLM provider selection, such as Harbor,
uses `openai/trapi/gpt-5.2_2025-12-11` with this gateway's base URL.

The gateway binds **loopback only**. Remote access is a separate transport
concern: an SSH forward or authenticated tunnel can expose this same port.
Deploying this project does not open an external port or create a tunnel.

## Configuration

Set these before deployment; non-secret settings are persisted in the generated
systemd units. For user-assigned identity, set `AZURE_CLIENT_ID`.

| Setting | Default |
| --- | --- |
| `TRAPI_BASE_URL` | `https://trapi.research.microsoft.com/redmond/interactive` |
| `TRAPI_SCOPE` | `api://trapi/.default` |
| `TRAPI_API_VERSION` | `2025-04-01-preview` |
| `TRAPI_CATALOG_VERSION` | `preview` |
| `--port` / `TRAPI2LITELLM_PORT` | `4000` |
| `--config-dir` / `TRAPI2LITELLM_CONFIG_DIR` | `$XDG_CONFIG_HOME/litellm-trapi`, normally `~/.config/litellm-trapi` |
| `--state-dir` / `TRAPI2LITELLM_STATE_DIR` | `$XDG_STATE_HOME/trapi2litellm`, normally `~/.local/state/trapi2litellm` |

Discovery calls `<TRAPI_BASE_URL>/openai/models?api-version=preview`. The
generation and discovery API versions are separate: the literal `preview`
returned 404 for generation in the initial deployment, while the dated version
worked. Keep the dated deployment ID in requests; no moving model aliases are
created. The real model family from TRAPI metadata is supplied to LiteLLM for
parameter validation without rewriting that deployment ID.

## Synchronization and limits

An hourly systemd timer fetches the catalog through Managed Identity. It
generates a deterministic config and does nothing to the gateway if unchanged.
When changed, it retains the old config, atomically publishes the new config,
and sends Gunicorn HUP. Existing requests have up to 900 seconds to finish;
upstream requests are limited to 600 seconds. The updater verifies the new
model list and `X-TRAPI-Config-SHA256`, and restores the previous config if that
check fails.

- Empty, malformed, duplicate-ID or paginated responses fail closed.
- A catalog reduction over 25% requires manual review; the old config remains.
- `provisioningState: Succeeded` is a catalog signal, **not** an inference probe.
  The API preserves capability metadata and explicitly marks this distinction.
- Models with unknown capabilities remain marked unknown. Not every listed
  deployment supports Chat Completions; use the appropriate endpoint.
- No cross-model fallback or silent parameter dropping is configured. Client
  code controls retries. An upstream retirement cannot be prevented locally.
- Basic smoke tests do not certify every model, parameter or token-expiry cycle.

## Operations and tests

```sh
systemctl --user status litellm-trapi.service
systemctl --user list-timers litellm-trapi-sync.timer
systemctl --user start litellm-trapi-sync.service  # discover and sync now
systemctl --user reload litellm-trapi.service     # replace workers gracefully
journalctl --user -u litellm-trapi.service -u litellm-trapi-sync.service

uv run --frozen ruff check .
uv run --frozen python -m unittest discover -v    # offline, no credentials needed
uv run --frozen python smoke_test.py             # small billable synthetic requests
```

The live smoke test checks missing/wrong keys, catalog/status routes, GPT-5.2
chat, GPT-4o-mini streaming, a tool-call round trip, Responses, and a worker
reload during an active stream. It requires those deployments and saves a
report in the state directory. For non-default deployment settings, export the
same `TRAPI2LITELLM_*` variables when running scripts directly, or trigger the
generated sync unit.

Secrets (`gateway.env`, mode 0600), generated configuration, catalog snapshots,
sync reports and previous/rejected configs live **outside Git**. The deployer
rejects config/state directories within the source checkout. The checked-in
files are application code, tests, deployment logic and dependency locks only.

## Source layout

- `sync_models.py`: discovery, config generation, validation and publication
- `gateway_app.py`: LiteLLM ASGI integration, key gate, catalog and status routes
- `settings.py`: environment-based paths and upstream settings
- `deploy.py`: parameterized user-service units, client setup and installation
- `test_*.py`: offline regression tests
- `smoke_test.py`: bounded live gateway acceptance checks

LiteLLM and Azure Identity implement the model protocols and credential refresh;
this project does not implement an alternative inference proxy.
