# Distribution status

Execution home: [P-866](https://linear.app/acturea/issue/P-866).
This file owns the measured packaging evidence; channel integration is P-867.

## Accepted source and artifacts (2026-10-04)

Version: `0.1.0`. License: `AGPL-3.0-only`, confirmed by the user; the wheel
contains the full LICENSE and SPDX License-Expression / License-File metadata.

Accepted source: `6d285cf2e2c97d750b82db8aca375d2309ac1c16`, pushed to
`feature/p-866-建立安装包-cli-和稳定服务入口，发行-wheelsdist-与-debian-包`.
[CI run 37136435242](https://github.com/Acture/trapi2litellm/actions/runs/37136435242)
completed successfully with all eight jobs passing. Five uploaded artifacts
were verified present and unexpired at acceptance: wheel/sdist and four native
Debian packages. This ledger update does not change the accepted source.

| Target | Verified behavior |
| --- | --- |
| Python 3.12 and 3.13 | Frozen dependencies, lint/types, 38 offline tests, wheel/sdist build |
| Ubuntu amd64 and arm64 | Independent wheel/sdist installs after source removal; uvx preview and temporary-deploy refusal; persistent uv tool user units; real gateway start and HUP/reload |
| Debian 13 amd64 and arm64 (Python 3.13) | Native .deb build; offline install, user-service start, upgrade/restart, remove, purge and reinstall |
| Ubuntu 24.04 amd64 and arm64 (Python 3.12) | Native .deb build; offline install, user-service start, upgrade/restart, remove, purge and reinstall |

Distribution checks probe missing/wrong-key rejection and authenticated
model/catalog/status/info endpoints using a synthetic catalog. Reload checks
wait for the new configuration hash. They do not authenticate to Azure or call
inference. Debian lifecycle tests run with Docker networking disabled and prove
that installation does not create a user key/configuration or start a service;
upgrade/removal retain the user's key and state. The upgrade fixture uses an
earlier Debian revision of the same payload, not a previously published release.

Debian packages constrain the system Python minor, embed hashed locked application
dependencies, derive ELF system dependencies and declare systemd,
dbus-user-session and procps. Maintainer scripts do not download dependencies,
authenticate or start services. These are vendored application packages, not
Debian archive submissions.

## Local verification (2026-10-03–04, macOS arm64)

- Python 3.12 and 3.13 wheel/sdist installations from a removed disposable
  source copy passed included offline tests and actual foreground gateway/HUP
  probes. uvx and isolated uv tool checks passed; the local tool directories
  were temporary, with real persistent deployment verified separately in CI.
- Final source regression: 38 offline tests, ruff check / format, ty and
  actionlint passed. Local wheel/sdist were rebuilt from the accepted source.
- Docker was not running locally; native Linux package acceptance was performed
  by the CI jobs above.

## Delivery boundaries

- Source and CI artifacts are available; no PR, tag, GitHub Release, PyPI
  publication, Homebrew formula, signed apt source or real host deployment.
- Homebrew and signed apt delivery are P-867. AGPL and packaging work retain
  Python + LiteLLM; a Go/Rust proxy would be a separate implementation decision.
- Live Managed Identity, inference, streaming during reload and credential-expiry
  checks remain separate from this synthetic packaging acceptance.

## Rust gateway compatibility probe (2026-10-04)

The user requested an isolated evaluation without replacing LiteLLM. The
[AISIX experiment](experiments/aisix/README.md) pins release `v1.5.0` at
`26497758704c28f62bb9d1d763886140691a365b`. Upstream source hashes were verified;
the unchanged Azure URL resolver/validator was compiled with Rust 1.96.1 in a
dependency-free test crate. Only its error container was substituted.

Five requirement assertions ran: one passed and four failed. TRAPI's base path
is preserved, but the existing dotted deployment ID is rejected, the generation
API version differs, the attempted query override is rejected, and the source
token-audience constant differs. The last assertion checks a constant, not
credential acquisition. Native Azure auth source inspection additionally shows
application client credentials/API keys rather than Managed Identity.

Probe code passed ruff check/format, ty, rustfmt and Cargo Clippy with warnings
denied. This rejects the released native Azure adapter as a direct replacement;
it does not establish full gateway behavior or rule out other adapter setups.
No full gateway build/start, Azure authentication, token-expiry cycle or inference
was performed. A manual full-build command is recorded in the experiment.
Production code, dependencies and the accepted packaging source remain unchanged.
`trapi-bridge` is only a naming proposal, awaiting a user decision.

## Runtime reuse preference (2026-10-04)

The user prefers reusing an existing system uv/Python over carrying another
runtime. The recommended wheel-install commands now select system Python
3.12/3.13 explicitly and disable managed Python/automatic interpreter downloads.
Command options were verified against local uv 0.12.17; offline system discovery
selected `/opt/homebrew/opt/python@3.13/bin/python3.13`. No installation or download
was performed for this documentation change.

The existing Debian builder already creates its environment from
`/usr/bin/python3` with downloads disabled; it carries locked application
dependencies, not a second Python distribution or uv executable. Persistent
`uv tool install` remains the service route; cache-based `uvx` remains for
temporary commands/foreground serving. Packaging implementation and previously
accepted artifact evidence are unchanged.
