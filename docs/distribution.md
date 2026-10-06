# Distribution and acceptance

Run the following commands from the repository root. See the
[installation guide](../README.md#install) for the persistent wheel/system-uv route
and [STATUS.md](../STATUS.md) for measured acceptance evidence.

## Wheel and sdist acceptance

Wheels contain a native Rust executable and Python runtime code. Build each
wheel on its target architecture; a prebuilt wheel installs without a Rust
compiler. Building the sdist requires Rust 1.89 or newer. Both installation
routes still need Python 3.12/3.13 and the locked LiteLLM/Azure SDK dependencies.

`packaging/check_dist.py` builds from a disposable copy, deletes that source,
installs wheel and sdist separately with hashed locked dependencies, runs the
included offline tests, probes the real foreground gateway using a synthetic
model, and checks uvx refusal/uv tool unit paths. It does not call inference.

```fish
uv run --frozen python packaging/check_dist.py --python python3.12
```

## Debian and Ubuntu packages

The Debian builder runs **on the target distribution and architecture** and
embeds locked dependencies in `/opt/trapi2litellm`. The public command is
`/usr/bin/trapi2litellm`, a stable symlink to
`/opt/trapi2litellm/bin/trapi2litellm`; the native binary finds Python in that
same environment. The package declares the matching system Python minor version
and derives ELF shared-library dependencies
for both the CLI and native Python dependencies with `dpkg-shlibdeps`. No package
maintainer scripts download dependencies, authenticate, or start a service.
The package reuses `/usr/bin/python3`: it contains application dependencies,
not a second Python distribution or a bundled `uv`. `uv` is a build prerequisite
only. Choose the wheel/system-uv route for a smaller download when online
dependency installation is acceptable; the Debian package carries its
dependencies for offline installation.

```fish
# In a prepared Debian 13 / Ubuntu 24.04 build environment:
python3 packaging/build_deb.py dist/trapi2litellm-0.1.0.tar.gz --out debs
# Install the artifact built for your distro and architecture:
sudo apt install ./debs/trapi2litellm_VERSION_ARCH.deb
```

Build prerequisites and the pinned uv builder are in
[packaging/Dockerfile](https://github.com/Acture/trapi2litellm/blob/main/packaging/Dockerfile).
CI checks Debian 13 / Ubuntu 24.04 × amd64 / arm64; installation, upgrade,
remove/purge and reinstall run with Docker networking disabled. These are
third-party application packages with vendored Python dependencies, not Debian
archive submissions.

Run the longer Linux checks manually on the target host. A persistent tool
acceptance directory must be outside source, temporary and cache directories:

```fish
uv run --frozen python packaging/check_dist.py --python python3.12 --tool-root "$HOME/.local/share/trapi2litellm-acceptance"
docker build --build-arg BASE=debian:13 -f packaging/Dockerfile -t trapi2litellm-builder .
docker run --rm --volume "$PWD:/src" trapi2litellm-builder python3 packaging/build_deb.py dist/trapi2litellm-*.tar.gz --out debs
```

The workflow in `.github/workflows/ci.yml` includes native arm64 and amd64
builders and offline Debian lifecycle checks. Consult STATUS for which checks
have actually run against the current migration.

## Live Azure acceptance

Regular CI uses GitHub-hosted runners and offline fixtures; it does not require
an Azure runner or Managed Identity. Mocked tests and synthetic gateway checks
cannot establish real Azure authentication, inference or credential refresh.
The offline contract tests cover API-key rejection, unbuffered SSE frame
forwarding and using each request's current SDK-supplied catalog token.
Real Azure acceptance remains optional manual work in an existing Azure environment,
recorded separately from the packaging CI result.

## Upgrade and removal

Before uninstalling a running installation, stop its user units:

```fish
systemctl --user disable --now litellm-trapi.service litellm-trapi-sync.timer
systemctl --user stop litellm-trapi-sync.service
```

Remove the three generated units from `~/.config/systemd/user` and run
`systemctl --user daemon-reload`, then uninstall the tool/package. User keys,
configuration and state are retained by package removal and purge. Upgrading a
package leaves a running gateway on its current workers; run `deploy --start`
to validate the new installation and restart it.

`deploy --start` holds the synchronization lock through activation. On failure
it restores the previous configuration, catalog/sync status, generated units
and client files, then restores the gateway/timer activation and enablement.
An existing gateway must pass readiness again with its previous model list
and configuration hash. A failed first deployment removes the new configuration
and units and stops/disables the new services; it retains the private key for
retry. Rejected configurations and `.previous` backups are retained.

`deployment-status.json`, also exposed under `/status`, records `ready`,
`rolled_back` or `rollback_failed` and the failing stage separately from sync
history. Recovery errors are reported instead of claiming successful rollback.
This handles command failures, not abrupt process termination. Stop an active
gateway before changing its configuration directory or port so that rollback
has a known readiness endpoint, or when replacing an older unit layout that
does not record those settings in the generated form. A newly enabled linger setting is undone on
failure; an existing linger setting is retained.

The installed-distribution CI runs real user-systemd rollback checks for first
deployment and redeployment on both architectures. These use a synthetic catalog
and an inert sync-service fixture; they never authenticate to Azure.
