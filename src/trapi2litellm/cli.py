"""Installed CLI; help and version need neither Azure credentials nor systemd."""

import argparse
import subprocess
import sys
from collections.abc import Sequence
from importlib.metadata import version


def run(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="trapi2litellm",
        description="Managed-identity TRAPI discovery and a loopback LiteLLM gateway",
    )
    parser.add_argument(
        "--version", action="version", version=f"%(prog)s {version('trapi2litellm')}"
    )
    commands = parser.add_subparsers(dest="command", required=True)
    for name, description in {
        "deploy": "install user units; --start explicitly enables and starts services",
        "serve": "run the gateway in the foreground using the installed Python environment",
        "sync": "discover models and atomically update the configuration",
        "smoke-test": "run bounded live acceptance checks (billable inference)",
    }.items():
        commands.add_parser(name, help=description, add_help=False)
    args, remaining = parser.parse_known_args(argv)
    if args.command == "deploy":
        from trapi2litellm.deploy import main
    elif args.command == "serve":
        from trapi2litellm.server import main
    elif args.command == "sync":
        from trapi2litellm.sync_models import main
    else:
        # The existing live test has no options, but still provide safe help.
        smoke = argparse.ArgumentParser(
            prog="trapi2litellm smoke-test", description="Billable live gateway checks"
        )
        smoke.parse_args(remaining)
        from trapi2litellm.smoke_test import main as smoke_main

        smoke_main()
        return 0
    return main(remaining)


def main() -> int:
    try:
        return run()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        return 1
