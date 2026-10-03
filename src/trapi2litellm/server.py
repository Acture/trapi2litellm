"""Run Gunicorn with this installation's interpreter and namespaced ASGI app."""

import argparse
import os
import sys
from collections.abc import Sequence


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="trapi2litellm serve", description=__doc__)
    parser.add_argument("--port", type=int)
    args = parser.parse_args(argv)
    if args.port is not None:
        os.environ["TRAPI2LITELLM_PORT"] = str(args.port)
    from trapi2litellm import settings

    os.environ.setdefault("CONFIG_FILE_PATH", str(settings.CONFIG_PATH))
    os.environ.setdefault("LITELLM_LOCAL_MODEL_COST_MAP", "True")
    os.environ.setdefault("LITELLM_MODE", "PRODUCTION")
    os.environ.setdefault("LITELLM_LOG", "WARNING")
    os.environ.setdefault("AZURE_TOKEN_CREDENTIALS", "ManagedIdentityCredential")
    os.environ.setdefault("AZURE_CREDENTIAL", "DefaultAzureCredential")
    if not os.environ.get("LITELLM_MASTER_KEY"):
        from trapi2litellm.sync_models import local_key

        os.environ["LITELLM_MASTER_KEY"] = local_key()
    command = [
        sys.executable,
        "-m",
        "gunicorn",
        "trapi2litellm.gateway_app:app",
        "--bind",
        f"127.0.0.1:{settings.PORT}",
        "--workers",
        "2",
        "--worker-class",
        "uvicorn_worker.UvicornWorker",
        "--timeout",
        "120",
        "--graceful-timeout",
        "900",
        "--keep-alive",
        "5",
        "--error-logfile",
        "-",
        "--log-level",
        "warning",
    ]
    os.execv(sys.executable, command)
    return 0
