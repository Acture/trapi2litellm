# Distribution status

Current execution home: [OSS-306](https://linear.app/acturea/issue/OSS-306), Rust
control migration. Earlier Python packaging acceptance belongs to
[OSS-76](https://linear.app/acturea/issue/OSS-76), formerly P-866.
This file owns measured packaging evidence; channel integration is P-867.

## Rust control migration (2026-10-04, macOS arm64)

The native CLI owns configuration, synchronization, publication/rollback and
systemd deployment. Python remains the Azure SDK, LiteLLM schema and gateway
runtime. Maturin builds one platform wheel containing both; Debian staging keeps
the native binary beside its Python environment and scans both for ELF libraries.

Local source checks passed: Rust formatting, Clippy and 18 offline tests;
Python ruff check/format, ty and 11 offline tests; actionlint and frozen lock
validation. Subprocess tests cover deadlines, child reaping and large stdin,
stdout and stderr. Normal synchronization rollback is covered independently.

Actual local artifacts were built with the **dev** profile, not release:
`trapi2litellm-0.1.0-py3-none-macosx_11_0_arm64.whl` and
`trapi2litellm-0.1.0.tar.gz`. Separate temporary environments installed the
wheel and compiled the extracted sdist. Both passed the included Python tests
and real foreground gateway/auth/model/status/HUP probes using a synthetic
catalog, including hostile working-directory/PYTHONPATH fixtures. The archives
contain the native CLI, Python runtime and full license; private notes,
experiments and agent scratch are excluded. These are local development
artifacts, not published releases.

Linux release wheels, Python 3.13 execution, native systemd deployment and the
four Debian/Ubuntu targets have **not** been rerun for this Rust migration.
The earlier acceptance below applies to the Python implementation only.
Manual long-check commands are in [docs/distribution.md](docs/distribution.md).
Rust 1.89 is the declared minimum; local checks used 1.98.1, and CI/build images
pin 1.99.0. No minimum-toolchain execution is claimed.

Explicit `deploy --start` retains the prior activation behavior: a failure after
publishing configuration may leave the new configuration in place. Full
deployment activation rollback is tracked separately in
[OSS-309](https://linear.app/acturea/issue/OSS-309). Live Managed Identity,
inference, streaming during reload and credential expiry remain untested here.

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

- Source and CI artifacts are available; no tag, GitHub Release, PyPI
  publication, Homebrew formula, signed apt source or real host deployment.
- Pull-request review and delivery links are tracked in the Linear execution home.
- Homebrew and signed apt delivery are P-867.
- Live Managed Identity, inference, streaming during reload and credential-expiry
  checks remain separate from this synthetic packaging acceptance.

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

## Pull-request preparation checks (2026-10-04)

At PR preparation, local ruff check/format, ty, actionlint and all 38 offline
regression tests passed. The accepted CI run was rechecked successfully, and
application and packaging implementation files matched the accepted source.
The target branch was `main`; the earlier accepted run establishes the
packaging evidence recorded above.
