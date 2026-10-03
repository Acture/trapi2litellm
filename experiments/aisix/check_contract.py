"""Compile pinned AISIX URL code against TRAPI requirements, without Cargo downloads."""

import argparse
import hashlib
import subprocess
import tempfile
from pathlib import Path

COMMIT: str = "26497758704c28f62bb9d1d763886140691a365b"
FILES: dict[str, str] = {
    "crates/aisix-provider-azure-openai/src/bridge.rs": (
        "62d3999d909c71c58ecad16e0bfa900756d1b2910edc22e9d11953f32df2a5a1"
    ),
    "crates/aisix-provider-azure-openai/src/aad_token_mint.rs": (
        "722208b629b3fb1ee13fd344249d36a63e060178e7e3f5aa3f8032665e679c81"
    ),
}

# Only the error container is substituted. The upstream URL resolver and
# validation functions are compiled verbatim; no gateway or credential SDK is mocked.
PRELUDE: str = """
mod aisix_gateway {
    pub enum BridgeError { InvalidUpstreamConfig(String) }
    impl std::fmt::Debug for BridgeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::InvalidUpstreamConfig(message) => f.debug_tuple("InvalidUpstreamConfig")
                    .field(message).finish(),
            }
        }
    }
}
use aisix_gateway::BridgeError;
"""


def pinned_source(source: Path, relative: str) -> str:
    data = (source / relative).read_bytes()
    if hashlib.sha256(data).hexdigest() != FILES[relative]:
        raise ValueError(f"Source differs from AISIX v1.5.0 ({COMMIT}): {relative}")
    return data.decode()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="Extracted AISIX v1.5.0 source directory")
    parser.add_argument("--toolchain", help="Already installed rustup toolchain")
    args = parser.parse_args()
    bridge = pinned_source(args.source, "crates/aisix-provider-azure-openai/src/bridge.rs")
    mint = pinned_source(args.source, "crates/aisix-provider-azure-openai/src/aad_token_mint.rs")
    start = bridge.index("#[derive(Debug, Clone, PartialEq, Eq)]\n#[non_exhaustive]")
    end = bridge.index("/// Discriminated auth scheme.", start)
    resolver = bridge[start:end]
    scope = next(line for line in mint.splitlines() if line.startswith("const AZURE_OPENAI_SCOPE:"))
    requirements = Path(__file__).with_name("requirements.rs").read_text()
    compiler = ["rustc", *([f"+{args.toolchain}"] if args.toolchain else [])]
    subprocess.run([*compiler, "--version"], check=True)
    print(f"AISIX v1.5.0 / {COMMIT}; source hashes verified", flush=True)
    with tempfile.TemporaryDirectory(prefix="trapi-aisix-contract-") as temp:
        root = Path(temp)
        entry = root / "contract.rs"
        binary = root / "contract"
        entry.write_text(PRELUDE + resolver + scope + "\n" + requirements)
        manifest = root / "Cargo.toml"
        manifest.write_text(
            '[package]\nname = "trapi-aisix-contract"\nversion = "0.0.0"\n'
            'edition = "2021"\n[lib]\npath = "contract.rs"\n[workspace]\n'
        )
        subprocess.run(
            [
                "cargo",
                *([f"+{args.toolchain}"] if args.toolchain else []),
                "clippy",
                "--offline",
                "--manifest-path",
                str(manifest),
                "--tests",
                "--",
                "-D",
                "warnings",
            ],
            check=True,
        )
        subprocess.run(
            [*compiler, "--edition=2021", "--test", str(entry), "-o", str(binary)], check=True
        )
        # A nonzero test result is the compatibility finding. Compiler and
        # preparation failures propagate independently, rather than looking like gaps.
        result = subprocess.run([str(binary), "--test-threads=1"], check=False)
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
