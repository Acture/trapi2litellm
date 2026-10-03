# Distribution status

Execution home: [P-866](https://linear.app/acturea/issue/P-866).
This file owns the measured packaging evidence; channel integration is P-867.

## Measured locally (2026-10-03, macOS arm64)

- Python 3.12: wheel and sdist built from a disposable source copy, copy removed,
  artifacts installed into independent environments and 37 offline tests passed
  from the sdist's test files.
- Python 3.12: uvx help/version/preview work and real deployment is refused;
  isolated uv tool installation retains its PATH-visible command in rendered
  units. Local tool directories are temporary, so actual persistent deployment
  is reserved for the Linux CI check.
- Actual foreground gateway: synthetic catalog, missing/wrong-key rejection,
  authenticated model/catalog/status/info endpoints, configuration hash change
  after Gunicorn HUP. No identity authentication or inference was requested.
- ruff check, ruff format, ty check and actionlint passed.
- Python 3.13: wheel/sdist installation with included offline tests, the actual
  installed foreground gateway, HUP, uvx and isolated uv tool checks passed.
- Final source regression: 38 offline tests passed, including install-only
  deployment refusing to authenticate, enable/start services or create a key.
  Wheel/sdist were rebuilt after these final source changes.
- User confirmed AGPL-3.0-only. LICENSE and SPDX package metadata are present.

## In flight / not yet accepted

- Linux CI owns real persistent uv tool unit installation, and Debian 13 /
  Ubuntu 24.04 × amd64 / arm64 .deb building and offline lifecycle acceptance.
  Docker is not running locally. Adding the workflow is not passing evidence.
- Debian packages carry the target system Python minor constraint, locked private
  application dependencies and derived ELF dependencies. Install-time scripts
  do not fetch dependencies or start services. These are application packages
  with vendored dependencies, not Debian archive submissions.
- Source changes are not yet committed or pushed. No PR, tag, GitHub Release,
  PyPI publication, Homebrew formula, signed apt source or real host deployment.
- The live Managed Identity/catalog/inference checks remain separate from
  packaging acceptance.
