# trapi2litellm

TRAPI model discovery and a local LiteLLM gateway, using Azure Managed Identity.
One port serves the model APIs, catalog and sync status. No Azure CLI login,
stored upstream bearer token, database, or separate catalog server is needed.

License: [AGPL-3.0-only](LICENSE).

## Install

Python 3.12 and 3.13 are supported. Build the wheel and sdist, then install the
CLI persistently (the checkout is only needed to build):

```fish
uv build
uv tool install dist/trapi2litellm-0.1.0-py3-none-any.whl
trapi2litellm --help
trapi2litellm --version
```

For a temporary invocation of the built artifact:

```fish
uvx --from ./dist/trapi2litellm-0.1.0-py3-none-any.whl trapi2litellm deploy --dry-run
```

`uvx` can preview units or run the foreground gateway; real deployment refuses
temporary/cache environments and editable installs. Use `uv tool install`, a
system package or Homebrew for persistent units. The generated units retain the
stable installed command (including its symlink), not a versioned environment.
`--entry-point /absolute/path/to/trapi2litellm` selects an installation explicitly.

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
uv run --frozen ruff format --check .
uv run --frozen ty check
uv run --frozen python -m unittest discover -s tests -v  # offline, no credentials needed
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

`packaging/check_dist.py` builds from a disposable copy, deletes that source,
installs wheel and sdist separately with hashed locked dependencies, runs the
included offline tests, probes the real foreground gateway using a synthetic
model, and checks uvx refusal/uv tool unit paths. It does not call inference.

```fish
uv run --frozen python packaging/check_dist.py --python python3.12
```

The Debian builder runs **on the target distribution and architecture** and
embeds locked dependencies in `/opt/trapi2litellm`. The public command is
`/usr/bin/trapi2litellm`. It declares the matching system Python minor version
and derives ELF shared-library dependencies with `dpkg-shlibdeps`. No package
maintainer scripts download dependencies, authenticate, or start a service.

```fish
# In a prepared Debian 13 / Ubuntu 24.04 build environment:
python3 packaging/build_deb.py dist/trapi2litellm-0.1.0.tar.gz --out debs
# Install the artifact built for your distro and architecture:
sudo apt install ./debs/trapi2litellm_VERSION_ARCH.deb
```

Build prerequisites and the pinned uv builder are in `packaging/Dockerfile`.
CI checks Debian 13 / Ubuntu 24.04 × amd64 / arm64; installation, upgrade,
remove/purge and reinstall run with Docker networking disabled. See `STATUS.md`
for the measured state of these checks. These are third-party application
packages with vendored Python dependencies, not Debian archive submissions.

Before uninstalling a running installation, stop its user units:

```sh
systemctl --user disable --now litellm-trapi.service litellm-trapi-sync.timer
systemctl --user stop litellm-trapi-sync.service
```

Remove the three generated units from `~/.config/systemd/user` and run
`systemctl --user daemon-reload`, then uninstall the tool/package. User keys,
configuration and state are retained by package removal and purge. Upgrading a
package leaves a running gateway on its current workers; run `deploy --start`
to validate the new installation and restart it.

## Source layout

- `src/trapi2litellm/cli.py`: installed command with lazy subcommands
- `src/trapi2litellm/server.py`: foreground Gunicorn entry point
- `src/trapi2litellm/sync_models.py`: discovery, config validation and publication
- `src/trapi2litellm/gateway_app.py`: ASGI key gate, catalog and status routes
- `src/trapi2litellm/settings.py`: XDG paths and upstream settings
- `src/trapi2litellm/deploy.py`: user-service units, client setup and installation
- `tests/`: offline regression tests
- `packaging/`: artifact builders and distribution acceptance
- `src/trapi2litellm/smoke_test.py`: bounded billable live acceptance checks

LiteLLM and Azure Identity implement the model protocols and credential refresh;
this project does not implement an alternative inference proxy.
