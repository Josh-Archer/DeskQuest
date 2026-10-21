#!/usr/bin/env bash
# Run against an already-started host; never replaces another daemon or installs packages.
set -euo pipefail
exec python3 "$(dirname "${BASH_SOURCE[0]}")/hw_validate.py" "$@"
