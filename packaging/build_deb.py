"""Build an architecture-specific private venv .deb from the accepted sdist on Linux."""

import argparse
import gzip
import logging
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
from pathlib import Path

LOG = logging.getLogger(__name__)


def run(command: list[str], cwd: Path, *, capture: bool = False) -> str:
    LOG.info("Running %s", " ".join(command[:10]))
    started = time.monotonic()
    result = subprocess.run(
        command, cwd=cwd, check=True, text=True, stdout=subprocess.PIPE if capture else None
    )
    LOG.info("Finished in %.1fs", time.monotonic() - started)
    return result.stdout or ""


def shared_library_dependencies(stage: Path, work: Path) -> str:
    """Let dpkg derive system dependencies; bundled SONAMEs are self-dependencies."""
    objects: list[Path] = []
    for path in stage.rglob("*"):
        if path.is_file() and not path.is_symlink():
            with path.open("rb") as handle:
                if handle.read(4) == b"\x7fELF":
                    objects.append(path)
    if not objects:
        raise ValueError("Expected the native CLI and dependency wheels in the private environment")
    debian = work / "debian"
    debian.mkdir()
    (debian / "control").write_text(
        "Source: trapi2litellm\nSection: net\nPriority: optional\nMaintainer: Acture <acturea@gmail.com>\n\nPackage: trapi2litellm\nArchitecture: any\nDescription: TRAPI discovery and LiteLLM gateway\n"
    )
    local: set[str] = set()
    for path in objects:
        output = run(["readelf", "-d", str(path)], work, capture=True)
        if soname := re.search(r"\(SONAME\).*\[([^\]]+)\]", output):
            # Match both SONAME forms understood by dpkg-shlibdeps. Wheel
            # repair tools also use the second form for hashed private libs.
            if match := re.fullmatch(r"(.+)\.so\.(.+)", soname[1]) or re.fullmatch(
                r"(.+)-(\d.*)\.so", soname[1]
            ):
                local.add(f"{match[1]} {match[2]} trapi2litellm")
    (debian / "shlibs.local").write_text("\n".join(sorted(local)) + "\n")
    command = [
        "dpkg-shlibdeps",
        "-O",
        "-xtrapi2litellm",
        *[f"-l{path}" for path in sorted({item.parent for item in objects})],
        *[f"-e{path}" for path in objects],
    ]
    output = run(command, work, capture=True)
    (work / "shlibdeps.txt").write_text(output)
    return next(
        line.partition("=")[2] for line in output.splitlines() if line.startswith("shlibs:Depends=")
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sdist", type=Path)
    parser.add_argument("--out", type=Path, default=Path("debs"))
    parser.add_argument("--revision", default="1", help="Debian revision, e.g. 0~acceptance1 or 1")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    if not sys.platform.startswith("linux") or sys.version_info[:2] not in ((3, 12), (3, 13)):
        raise ValueError("Run with the target Debian/Ubuntu system Python 3.12 or 3.13")
    if not re.fullmatch(r"[0-9][A-Za-z0-9.+~]*", args.revision):
        raise ValueError("Invalid Debian revision")
    for executable in ("uv", "dpkg-deb", "dpkg-shlibdeps", "dpkg-architecture", "readelf"):
        if not shutil.which(executable):
            raise ValueError(f"Install the build prerequisite {executable}")
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="trapi2litellm-deb-") as folder:
        work = Path(folder)
        with tarfile.open(args.sdist.resolve()) as archive:
            archive.extractall(work / "source", filter="data")
        (source,) = (work / "source").iterdir()
        project = tomllib.loads((source / "pyproject.toml").read_text())["project"]
        license_path = source / "LICENSE"
        if not license_path.is_file() or not project.get("license"):
            raise ValueError(
                "An explicit project license and LICENSE file are required before packaging"
            )
        version = project["version"]
        distro = {
            line.partition("=")[0]: line.partition("=")[2].strip('"')
            for line in Path("/etc/os-release").read_text().splitlines()
            if "=" in line
        }
        if (distro["ID"], distro["VERSION_ID"]) not in (("debian", "13"), ("ubuntu", "24.04")):
            raise ValueError("Build on Debian 13 or Ubuntu 24.04")
        architecture = run(["dpkg-architecture", "-qDEB_HOST_ARCH"], work, capture=True).strip()
        if architecture not in ("amd64", "arm64"):
            raise ValueError("Only amd64 and arm64 are release targets")
        deb_version = f"{version}-{args.revision}+{distro['ID']}{distro['VERSION_ID']}"
        run(
            [
                "uv",
                "build",
                "--wheel",
                "--python",
                sys.executable,
                "--out-dir",
                str(work / "wheel"),
            ],
            source,
        )
        (wheel,) = (work / "wheel").glob("*.whl")
        requirements = work / "requirements.txt"
        run(
            ["uv", "export", "--frozen", "--no-dev", "--no-emit-project", "-o", str(requirements)],
            source,
            capture=True,
        )
        stage = work / "stage"
        venv = stage / "opt/trapi2litellm"
        run(
            ["uv", "venv", "--python", "/usr/bin/python3", "--no-python-downloads", str(venv)], work
        )
        python = str(venv / "bin/python")
        run(
            [
                "uv",
                "pip",
                "sync",
                "--python",
                python,
                "--require-hashes",
                "--only-binary",
                ":all:",
                str(requirements),
            ],
            work,
        )
        run(["uv", "pip", "install", "--python", python, "--no-deps", str(wheel)], work)
        with (venv / "bin/trapi2litellm").open("rb") as handle:
            if handle.read(4) != b"\x7fELF":
                raise ValueError("The package entry point must be the native Rust executable")
        # Keep the native CLI beside its Python interpreter. Other dependency
        # scripts have staging shebangs and are not public package commands.
        for path in (venv / "bin").iterdir():
            if not path.name.startswith("python") and path.name != "trapi2litellm":
                path.unlink()
        binary = stage / "usr/bin/trapi2litellm"
        binary.parent.mkdir(parents=True)
        binary.symlink_to("/opt/trapi2litellm/bin/trapi2litellm")
        # dpkg recognizes this as one package build tree, including its
        # private libraries, only once DEBIAN exists.
        control = stage / "DEBIAN"
        control.mkdir()
        dependencies = shared_library_dependencies(stage, work)
        minor = sys.version_info.minor
        doc = stage / "usr/share/doc/trapi2litellm"
        doc.mkdir(parents=True)
        shutil.copyfile(license_path, doc / "copyright")
        shutil.copyfile(source / "README.md", doc / "README.md")
        shutil.copyfile(source / "STATUS.md", doc / "STATUS.md")
        shutil.copytree(source / "docs", doc / "docs")
        shutil.copyfile(requirements, doc / "requirements.txt")
        changelog = f"trapi2litellm ({deb_version}) unstable; urgency=medium\n\n  * Package upstream {version} with an isolated locked application environment.\n\n -- Acture <acturea@gmail.com>  Sat, 03 Oct 2026 00:00:00 +0000\n"
        with gzip.open(doc / "changelog.Debian.gz", "wb") as handle:
            handle.write(changelog.encode())
        installed_size = (
            sum(
                path.stat().st_size
                for path in stage.rglob("*")
                if path.is_file() and not path.is_symlink()
            )
            // 1024
        )
        (control / "control").write_text(
            f"Package: trapi2litellm\nVersion: {deb_version}\nArchitecture: {architecture}\nSection: net\nPriority: optional\nMaintainer: Acture <acturea@gmail.com>\nInstalled-Size: {installed_size}\nDepends: python3 (>= 3.{minor}), python3 (<< 3.{minor + 1}), systemd, dbus-user-session, procps, {dependencies}\nHomepage: https://github.com/Acture/trapi2litellm\nDescription: TRAPI discovery (Managed Identity or gateway relay) and local LiteLLM gateway\n Isolated locked application dependencies; explicit systemd user deployment.\n"
        )
        destination = output / f"trapi2litellm_{deb_version}_{architecture}.deb"
        run(["dpkg-deb", "--root-owner-group", "--build", str(stage), str(destination)], work)
        shutil.copyfile(work / "shlibdeps.txt", output / f"{destination.stem}.shlibdeps.txt")
        LOG.info("Built %s (Python 3.%s, %s)", destination, minor, architecture)
        LOG.info("System dependencies: %s", dependencies)
        LOG.info(
            "Installed size: %s KiB; package: %s bytes", installed_size, destination.stat().st_size
        )


if __name__ == "__main__":
    main()
