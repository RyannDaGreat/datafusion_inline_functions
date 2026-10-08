#!/usr/bin/env bash
# Command. Check Rust, build distributions, and test the installed wheel in a clean environment.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mkdir -p .scratchpad/tmp
export UV_CACHE_DIR="$PWD/.scratchpad/uv-cache"
export TMPDIR="$PWD/.scratchpad/tmp"
unset CONDA_PREFIX
export VIRTUAL_ENV="$PWD/.venv"
export PYO3_PYTHON="$VIRTUAL_ENV/bin/python"
cargo fmt --check
cargo test --no-default-features
cargo clippy --no-default-features -- -D warnings
.venv/bin/maturin build --release --locked --out dist
.venv/bin/maturin sdist --out dist
.venv/bin/python -m twine check --strict dist/*
uv venv --clear .scratchpad/wheel-test
uv pip install --python .scratchpad/wheel-test/bin/python dist/*.whl 'datafusion>=54,<55' pytest fire
.scratchpad/wheel-test/bin/python -m pytest -v tests --pyargs datafusion_inline_functions
