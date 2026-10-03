# AISIX compatibility probe

The user authorized testing an existing Rust gateway while retaining LiteLLM.
No application dependency, CLI name, installed service or remote project name
was changed. The candidate is **AISIX v1.5.0**, the latest published release
checked on 2026-10-04, at commit
[`26497758704c28f62bb9d1d763886140691a365b`](https://github.com/api7/aisix/tree/26497758704c28f62bb9d1d763886140691a365b).

## What was executed

`check_contract.py` verifies the upstream files' SHA-256 hashes and extracts the
original `AzureUpstreamRef`, URL resolver and deployment validator verbatim.
It compiles them with Rust 1.96.1 and executes `requirements.rs`. Only the error
container is substituted so these dependency-free functions can run separately
from the full gateway. The token-audience assertion reads the upstream constant;
it does not execute credential acquisition. See [STATUS.md](../../STATUS.md)
for measured results.

| Requirement | Finding |
| --- | --- |
| Preserve the TRAPI base path | Resolver preserves `/redmond/interactive` |
| Preserve the existing deployment ID | `gpt-5.2_2025-12-11` is rejected because it contains a dot |
| Use `2025-04-01-preview` | Resolver selects the fixed `2024-10-21` version |
| Set the API version in `api_base` | The TRAPI URL with a query is rejected |
| Obtain a token for TRAPI | The source fixes the audience to `https://cognitiveservices.azure.com/.default`, whereas TRAPI uses `api://trapi/.default` |

The [pinned Azure bridge](https://github.com/api7/aisix/blob/26497758704c28f62bb9d1d763886140691a365b/crates/aisix-provider-azure-openai/src/bridge.rs#L277)
contains the resolver and validator. The
[pinned token implementation](https://github.com/api7/aisix/blob/26497758704c28f62bb9d1d763886140691a365b/crates/aisix-provider-azure-openai/src/aad_token_mint.rs#L58)
requires `tenant_id`, `client_id` and `client_secret`, then posts a
`client_credentials` grant. Its native Azure authentication path offers an API
key or these application credentials, rather than the current Managed Identity
flow. This is source inspection, not an executed SDK/token-expiry test.

AISIX implements
[`/v1/models`](https://github.com/api7/aisix/blob/26497758704c28f62bb9d1d763886140691a365b/crates/aisix-proxy/src/models.rs),
but that endpoint and model reloads were **not** tested in a full gateway process.
This probe is sufficient to reject the released native Azure adapter as a
direct replacement; it does not rule out a future release, upstream fixes, or
a separately verified generic-adapter configuration.

## Reproduce

The source snapshot used locally is at
`/private/tmp/trapi-aisix.P1SRdo/aisix-26497758704c28f62bb9d1d763886140691a365b`.
From this repository, run:

```fish
set task_source /private/tmp/trapi-aisix.P1SRdo/aisix-26497758704c28f62bb9d1d763886140691a365b
.venv/bin/python experiments/aisix/check_contract.py $task_source --toolchain 1.96.1
```

The current result is intentionally nonzero: TRAPI requirements fail against
the candidate. Source/hash/compilation failures propagate separately. The probe
runs offline and calls no Azure, inference or identity endpoint. It also runs
`cargo clippy --offline --tests -- -D warnings` on the extracted contract crate.

## Full gateway build, if continuing

There were no binary release assets. A full build involves downloading and
compiling the gateway dependency tree; per the user's preference, this step
was left for manual execution. The following uses the existing source snapshot
and an already installed toolchain, with `protoc` available:

```fish
set task_source /private/tmp/trapi-aisix.P1SRdo/aisix-26497758704c28f62bb9d1d763886140691a365b
set -x AISIX_BUILD_VERSION 1.5.0
set -x AISIX_BUILD_SHA 26497758704c28f62bb9d1d763886140691a365b
cargo +1.96.1 build --locked --manifest-path $task_source/Cargo.toml -p aisix-server --bin aisix
and $task_source/target/debug/aisix --version
```

Successful compilation would not resolve the incompatibilities above. Before
adoption, a separate runtime check would still need to establish authenticated
model listing, exact deployment mapping, Chat/Responses and streaming semantics,
Managed Identity refresh for TRAPI's audience, and safe configuration reloads.
No custom inference gateway or AISIX fork is proposed by this experiment.

## Naming

`trapi-bridge` is a proposed name for the Azure/TRAPI-to-local-endpoint glue,
independent of the gateway implementation. The user asked whether a rename
makes sense; no name was selected and no rename was performed. Rust remains a
language preference, and switching the gateway remains unselected.
