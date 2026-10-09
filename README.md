# trapi2litellm

TRAPI model discovery and a local LiteLLM gateway, using Azure Managed Identity.
The native Rust CLI owns synchronization and user-service deployment; the
installed Python environment runs LiteLLM and the Azure SDK.
One port serves the model APIs, dashboard, catalog and sync status. No Azure CLI
login, stored upstream bearer token, external database service, or separate
catalog server is needed. Local usage observations use embedded SQLite.

License: [AGPL-3.0-only](LICENSE).

## Install

Python 3.12 and 3.13 are supported. Prefer the existing system `uv` and a
compatible system Python. Building from source also requires Rust 1.89 or newer.
Build the platform-specific wheel and sdist, then install the CLI
persistently (the checkout is only needed to build):

```fish
uv build --no-managed-python --no-python-downloads --python '>=3.12,<3.14'
uv tool install --no-managed-python --no-python-downloads --python '>=3.12,<3.14' dist/trapi2litellm-*.whl
trapi2litellm --help
trapi2litellm --version
```

These flags reuse an existing Python rather than downloading another
interpreter; installation fails if a compatible version is missing. `uv`
manages an isolated, persistent environment for LiteLLM and the other application
dependencies. Those dependencies still need to be installed. The installed
Rust command runs directly, without invoking `uv` on every service start.
It locates Python beside the installed binary, including through a stable
tool/package symlink. Installing a prebuilt wheel requires no Rust compiler.

For a temporary invocation of the built artifact:

```fish
uvx --no-managed-python --no-python-downloads --python '>=3.12,<3.14' --from ./dist/trapi2litellm-*.whl trapi2litellm deploy --dry-run
```

`uvx` can preview units or run the foreground gateway; real deployment refuses
temporary/cache environments and editable installs. Use `uv tool install`, a
system package or Homebrew for persistent units. The generated units retain the
stable installed command (including its symlink), not a versioned environment.
`--entry-point /absolute/path/to/trapi2litellm` selects an installation explicitly.
`uvx` environments are disposable caches; systemd units use the persistent
installed command so cache cleanup cannot remove their application environment.

PyPI publication, the Homebrew formula and a signed apt repository are separate
delivery steps. The build/acceptance workflow produces wheel/sdist and per-target
`.deb` artifacts; artifact upload does not publish an apt repository. Channel
integration belongs to [P-867](https://linear.app/acturea/issue/P-867).

## Deploy on Linux

Prerequisites: an Azure host with a TRAPI-authorized managed identity, a working
`systemctl --user` session, and the persistently installed command. Installation
does not select an Azure identity, fetch a catalog, start services or enable
lingering. No external devtunnel CLI is needed by this gateway.

```sh
trapi2litellm deploy
trapi2litellm deploy --start --enable-linger
```

The linger option keeps the user service running after logout and starts it at
boot, and requires `--start`. If your machine requires an administrator for that operation, run
`sudo loginctl enable-linger YOUR_USER` separately, then deploy without the flag.
The deployment command does not install uv, grant TRAPI permissions or configure
firewalls. Re-run it after upgrading the installed package; existing local keys are kept.

Preview the generated units without writing files or making network calls:

```sh
trapi2litellm deploy --dry-run
```

The service imports the installed package and remains usable after the checkout
is removed. `deploy` alone installs units/client setup; `deploy --start` fetches
the catalog, creates the local key if absent and enables/starts the service and
timer. Unit/executable changes require a service restart; regular
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
Data and inference endpoints require the local gateway key in `Authorization: Bearer ...`.
This key is separate from the auto-refreshed Managed Identity credential.
Open **http://127.0.0.1:4000/gateway** and enter that key to view the dashboard.
Its empty page and script are public; model metadata and usage remain authenticated.
The browser keeps the key in page memory, without URL or persistent storage.

| Endpoint on port 4000 | Purpose |
| --- | --- |
| `/v1/models` | Configured deployment IDs |
| `/v1/chat/completions` | Chat, streaming and tool calling |
| `/v1/responses` | Responses API |
| `/catalog` | Cached original TRAPI discovery evidence and fetch time |
| `/model/info` | LiteLLM configuration and per-model metadata |
| `/status` | Running configuration hash, sync history and deployment outcome |
| `/gateway` | Model capabilities, upstream rate limits, sync times and live local usage |
| `/gateway/models` | The same authenticated model metadata and usage as JSON |

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

## Synchronization and rate limits

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

The dashboard refreshes every three seconds. It displays each model's original
capabilities and `RateLimits`, plus normalized requests/minute and tokens/minute.
As in the [TRAPI portal](https://trapi-portal.research.microsoft.com/models),
limit values of 0 or -1 mean no rate cap for that dimension; missing limits remain unknown.
These limits describe per-minute throughput, rather than a cumulative token
allocation. The gateway displays usage and rate caps without a quota balance.
Metadata comes from the worker's running configuration. Catalog fetch time and
successful configuration sync time are separate; a sync for another config hash
does not label an older worker as updated.

Usage covers Chat Completions, Completions, Responses, Embeddings and Messages
requests through this gateway, including failed or cancelled attempts. Local
worker counts are shared and survive reload/restart, with 24-hour retention.
The page shows request counts, input/output tokens, in-flight requests and the
last minute's consumption. Request windows use arrival time; completed token
windows use completion time, with in-flight estimates shown separately.
Reported response `usage` replaces estimates. Missing usage is approximated from
UTF-8 JSON/text bytes divided by four, rather than a model tokenizer. Multimodal,
malformed or over-1-MiB observation payloads can have unknown usage. Request and
response content and API keys are not persisted in the usage store. Workers that
stop heartbeating for 30 seconds have their unfinished requests marked interrupted.

These observations cover only this gateway. Other clients sharing the upstream
rate limits are invisible here, so local consumption does not establish the
upstream's full rate-window usage. Displaying limits does not enable gateway
rate-limit enforcement.

## Operations and tests

```sh
systemctl --user status litellm-trapi.service
systemctl --user list-timers litellm-trapi-sync.timer
systemctl --user start litellm-trapi-sync.service  # discover and sync now
systemctl --user reload litellm-trapi.service     # replace workers gracefully
journalctl --user -u litellm-trapi.service -u litellm-trapi-sync.service

uv run --frozen ruff check .
uv run --frozen ruff format --check .
uv run --frozen ty check
uv run --frozen python -m unittest discover -s tests -v  # offline, no credentials needed
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
trapi2litellm smoke-test                                # billable synthetic requests
```

The live smoke test checks missing/wrong keys, catalog/status routes, GPT-5.2
chat, GPT-4o-mini streaming, a tool-call round trip, Responses, and a worker
reload during an active stream. It requires those deployments and saves a
report in the state directory. For non-default deployment settings, export the
same `TRAPI2LITELLM_*` variables when calling the CLI directly, or trigger the
generated sync unit.

Secrets (`gateway.env`, mode 0600), generated configuration, catalog snapshots,
sync reports and previous/rejected configs live **outside Git**. The deployer
rejects config/state directories within the source checkout. The checked-in
files are application code, tests, deployment logic and dependency locks only.

## Distribution acceptance

See [distribution and acceptance](docs/distribution.md) for wheel/sdist checks,
Debian/Ubuntu package builds, and upgrade/removal instructions. Measured
acceptance evidence remains in [STATUS.md](STATUS.md).

## Source layout

- `src/*.rs`: native CLI, immutable settings, catalog policy, sync and user-service deployment
- `src/trapi2litellm/runtime.py`: internal Azure SDK/schema boundary and Gunicorn entry
- `src/trapi2litellm/gateway_app.py`: ASGI key gate, dashboard, catalog and status routes
- `src/trapi2litellm/model_view.py`, `usage.py`: running metadata and shared local usage observations
- `tests/`: offline regression tests
- `packaging/`: artifact builders and distribution acceptance
- `docs/`: public documentation
- `notes/`: optional private project-notes submodule; not required to build or run
- `src/trapi2litellm/smoke_test.py`: bounded billable live acceptance checks

LiteLLM and Azure Identity implement the model protocols and credential refresh;
this project does not implement an alternative inference proxy.
