#!/usr/bin/env bash
# Command. Recreate the dump-local development environment; never install into conda.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
command -v cargo
command -v uv
mkdir -p .scratchpad/tmp
export UV_CACHE_DIR="$PWD/.scratchpad/uv-cache"
export TMPDIR="$PWD/.scratchpad/tmp"
uv sync --group dev
ln -sfn ../../pypi .venv/bin/pypi
printf '\nReady. Activate with: source .venv/bin/activate\nRun tests with: ./check.sh\nCheck upload artifacts with: pypi --check\nPublish with: pypi\n'
