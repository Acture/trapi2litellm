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
