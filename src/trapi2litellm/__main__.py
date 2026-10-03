"""Support python -m trapi2litellm in the same environment as the console script."""

from trapi2litellm.cli import main

raise SystemExit(main())
