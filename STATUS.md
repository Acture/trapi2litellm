# Distribution status

Current execution home: [OSS-381](https://linear.app/acturea/issue/OSS-381), model
metadata and live local usage display. Rust control migration belongs to
[OSS-306](https://linear.app/acturea/issue/OSS-306). Earlier Python packaging acceptance belongs to
[OSS-76](https://linear.app/acturea/issue/OSS-76), formerly P-866.
This file owns measured packaging evidence; channel integration is P-867.

## Gateway model and usage display (2026-10-10)

The authenticated `/gateway/models` API supplies running model capabilities,
original and normalized upstream limits, catalog/config sync times and local
usage. `/gateway` is an empty dashboard shell; its data requires the gateway key,
kept only in browser memory. The page refreshes every three seconds.

SQLite aggregates local workers and retains observations for 24 hours, including
across HUP/restart. Response-reported input/output usage overrides estimates;
completed-window and in-flight estimates are separate. Unknown or multimodal
usage remains explicit. No prompt, reply or key is persisted in the usage store.
These observations do not establish billing, global quota remaining or enforcement.

Local acceptance: Python ruff/format/ty and 32 offline tests; TypeScript strict
checking of the typed dashboard script, ESLint, Prettier and actionlint.
Tests include two real processes sharing counts, rolling windows, interrupted
workers, cancellation, fragmented streaming, privacy and authenticated data.
The existing editable installation passed actual two-worker foreground startup,
metadata/page probes and HUP with seeded usage preserved; no upstream inference.
An independent headless Chromium check passed desktop/mobile layout, key
rejection/recovery, polling and memory-only key handling using synthetic API data.

Installed wheel/sdist, persistent systemd and Debian lifecycle acceptance now
probe the new page/assets, authenticated metadata and shared usage retention.
Full CI acceptance for this change is pending; earlier accepted source below
does not establish acceptance of this addition.

First source `c9defb5`, [CI 37956948460](https://github.com/Acture/trapi2litellm/actions/runs/37956948460),
passed both Python source jobs, dashboard checks, MSRV and amd64 installed
acceptance. Arm64's sdist server failed at concurrent SQLite WAL initialization
with `database is locked`; Debian jobs were skipped. Bootstrap now serializes
workers using a separate advisory initialization lock and avoids changing an existing
WAL journal. The multiprocessing regression starts from a fresh store. A late
completion after a worker lease expires also replaces its estimates with reported
usage. Subsequent full CI acceptance is pending.

## Final source acceptance (2026-10-07)

Final functional source: `39e297a4487f583b56bdccd111e75133afd0c211`, pushed to
`main`. Its [CI run 37496965153](https://github.com/Acture/trapi2litellm/actions/runs/37496965153)
completed successfully with all nine jobs passing. This ledger-only update
does not change that accepted functional source or rebuild its artifacts.

| Target | Verified behavior |
| --- | --- |
| Rust 1.89 | Locked compilation of all targets with the declared minimum compiler |
| Python 3.12 and 3.13 | Rust fmt/Clippy and 23 tests; Python ruff/format/ty and 16 offline tests; wheel/sdist builds |
| Ubuntu amd64 and arm64 | Independent installed wheel/sdist, uvx refusal, persistent uv tool, real systemd startup/HUP and first-deployment/redeployment rollback |
| Debian 13 and Ubuntu 24.04 × amd64 and arm64 | Native package build; network-disabled install, upgrade/restart, remove, purge and reinstall |

GitHub's artifact API verified five uploaded bundles (`python-dist` and
`deb-0` through `deb-3`) present and unexpired at acceptance; their reported
expiry is 2027-01-04. These are CI artifacts, not published package releases.

The real-systemd rollback test is ignored in source-test jobs and explicitly
run during installed-distribution acceptance on both architectures. Offline
contract tests cover API-key rejection, SSE delivery before upstream completion,
separate deployment/sync status and current SDK-supplied catalog tokens.

The last guard rejects active configuration/state-directory or port changes
before publishing deployment files. Local Rust fmt/Clippy and the four deployment
transaction tests passed. No Azure runner is required; live Azure authentication,
inference and credential-expiry recovery remain optional manual acceptance.

## Rust control migration (2026-10-04, macOS arm64)

The native CLI owns configuration, synchronization, publication/rollback and
systemd deployment. Python remains the Azure SDK, LiteLLM schema and gateway
runtime. Maturin builds one platform wheel containing both; Debian staging keeps
the native binary beside its Python environment and scans both for ELF libraries.

Local source checks passed: Rust formatting, Clippy and 18 offline tests;
Python ruff check/format, ty and 12 offline tests; actionlint and frozen lock
validation. Subprocess tests cover deadlines, child reaping and large stdin,
stdout and stderr. Normal synchronization rollback is covered independently.
The final caller-relative configuration fix reran the covering Rust CLI/runtime
tests and Python exec-boundary regression; both review findings were closed.

Actual local artifacts were built with the **dev** profile, not release:
`trapi2litellm-0.1.0-py3-none-macosx_11_0_arm64.whl` and
`trapi2litellm-0.1.0.tar.gz`. Separate temporary environments installed the
wheel and compiled the extracted sdist. Both passed the included Python tests
and real foreground gateway/auth/model/status/HUP probes using a synthetic
catalog, including hostile working-directory/PYTHONPATH fixtures and both
relative and absolute `CONFIG_FILE_PATH` overrides. The archives
contain the native CLI, Python runtime and full license; private notes,
experiments and agent scratch are excluded. These are local development
artifacts, not published releases.

Verified artifact source: `5dac34c51ec9dddd8320e6822640fd4e47d50169` on the local
`feature/rust-control` branch. Actual archive SHA-256:

| Artifact | SHA-256 |
| --- | --- |
| macOS arm64 dev wheel | `717b1b40e0bb83ca15081a01dbad8d7d72d912408c9188f8fd9ba2748ac62167` |
| sdist | `d81f95348d47f9676b7743c024dc2118fcb72656c249517568cbc940826004ab` |

Debian includes the public distribution guide and STATUS beside README;
native installation acceptance is established by the later Linux runs below.

The Rust migration was pushed to `main` on 2026-10-06 at `eed591d`.
[CI run 37471382076](https://github.com/Acture/trapi2litellm/actions/runs/37471382076)
passed the Python 3.12/3.13 source checks, offline tests and distribution builds.
Both installed-distribution jobs failed at systemd preflight: quoted
`EnvironmentFile` filenames were interpreted as non-absolute paths. The four
Debian lifecycle jobs were skipped. This does not establish Linux package
acceptance; the earlier acceptance below applies to the Python implementation.

The first follow-up, `0c96654`, passed systemd preflight in
[CI run 37476599165](https://github.com/Acture/trapi2litellm/actions/runs/37476599165),
but both service startup checks failed to load the key file: systemd's loader
also interprets backslashes and glob patterns. The next fix escapes literal
backslashes/glob characters as well as percent specifiers, and the persistent
tool fixture exercises these characters in a real service path.
Local deployment regression tests (3), including a real POSIX glob check that
selects the literal key file while excluding a matching decoy,
Rust fmt/Clippy and Python ruff/format/ty checks passed. The later Linux run
below establishes systemd and Debian lifecycle acceptance for this fix.

The `d590294` CI rerun
[37487261765](https://github.com/Acture/trapi2litellm/actions/runs/37487261765)
passed the literal-path glob regression, but Python 3.12's Rust runtime protocol
fixture intermittently failed with `ETXTBSY` while executing a newly written
script, preventing distribution jobs from running. The fixture now exercises
the same JSON subprocess boundary through `/bin/sh -c`, without executing a
freshly writable fixture inode. No subprocess retry policy was added.
At `8e4e1d7`, [CI run 37489996978](https://github.com/Acture/trapi2litellm/actions/runs/37489996978)
passed all eight jobs: Python 3.12/3.13, both Linux installed-distribution jobs
and all four Debian/Ubuntu lifecycle targets. This includes real persistent
user-service startup/reload with literal special-character paths, plus offline
package install, upgrade/restart, removal, purge and reinstall. This acceptance
belongs to `8e4e1d78ba78412a51de54fe4ddb751aa651ea43`; it does not cover the
additional deployment-rollback checks in the next source change.

Manual long-check commands are in [docs/distribution.md](docs/distribution.md).
Rust 1.89 is the declared minimum; local checks used 1.98.1, and CI/build images
pin 1.99.0. The dedicated minimum-compiler job passed `cargo +1.89.0 check
--locked --all-targets` at `1e8a793` in CI run 37492074434.

Deployment activation rollback is tracked in
[OSS-309](https://linear.app/acturea/issue/OSS-309); the follow-up below implements
it and passed real Linux acceptance. Live Managed Identity, inference,
streaming during reload and credential expiry remain untested here.
Regular CI uses offline fixtures on GitHub-hosted runners. No Azure runner is
required; real Azure acceptance is optional manual work in an existing environment.

## Deployment activation rollback follow-up (2026-10-06)

`deploy --start` now holds the synchronization lock through installation,
publication, activation and recovery. Failure restores old configuration,
catalog/sync metadata, generated units and client files, and restores gateway
and timer enablement/activation. Recovery checks the previous model set/hash
and service states. First-deployment failure stops/disables new units, removes
new configuration/units and retains the private key for retry. Failed
configuration and `.previous` backups are kept. A changed linger setting is
restored. Recovery failures remain explicit `rollback_failed` outcomes.

Local Rust verification passed 23 tests (20 unit + 3 CLI); the isolated real
systemd test is explicitly ignored locally. Python's 12 offline tests, Rust
fmt/Clippy, Python ruff/format/ty and actionlint passed. Fault injection covers
installation reload, catalog/validation, partial enable, linger, restart,
reload and readiness failures for first deployment and existing gateways;
recovery readiness and rejected-artifact write failures are also exercised.
Tests verify exact previous bytes/permissions, metadata and activation state,
key retention, terminal status and lock ownership during readiness.

The installed-distribution job explicitly runs the ignored real-systemd test
after normal service acceptance, using the installed stable CLI. It checks
first-deployment partial activation and failed redeployment, including recovery
of an actual gateway's old models/hash and mixed runtime/persistent enablement.
The synthetic catalog and inert timer sync fixture require no Azure identity.
This additional acceptance passed on both architectures at
`1e8a793160a74c9c03aa8a3b9ff9ecee55d3192c` in
[CI run 37492074434](https://github.com/Acture/trapi2litellm/actions/runs/37492074434).
All nine jobs passed for that source, including the four Debian/Ubuntu lifecycle
targets and the declared minimum Rust compiler.

`deployment-status.json` records the attempt independently from restored sync
history and is exposed under `/status`. Abrupt process termination is outside
this command-failure recovery. Active gateway configuration/state-directory or
port changes, and incompatible old unit layouts, are rejected before publication;
stop the gateway first for those changes.

The final offline contract follow-up adds auth rejection before upstream
execution, SSE frame delivery before upstream completion, deployment/sync status
separation and current SDK-supplied catalog tokens across successive requests.
Local Python verification passed 16 tests with ruff/format/ty; these mocks
do not establish real Azure authentication or token-expiry recovery. All nine
jobs passed at `13a591e3eacee00b4ed76a77a58384af371c9a7f` in
[CI run 37494655083](https://github.com/Acture/trapi2litellm/actions/runs/37494655083).
The subsequent active state-directory guard passed local Rust fmt/Clippy and
the four deployment transaction tests, followed by complete Linux acceptance
at the final functional source recorded above.

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
