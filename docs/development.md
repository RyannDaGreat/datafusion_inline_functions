# Development and publishing

Install Rust (`rustup`) and `uv`. From the repository root:

```sh
./setup.sh
source .venv/bin/activate
./check.sh
```

The setup creates a local environment and exposes a `pypi` command in that environment. All temporary files stay under `.scratchpad/`. `check.sh` runs Rust checks, builds wheel/sdist artifacts, checks metadata, installs the wheel into a clean environment, and runs the Python/DataFusion suite.

The GitHub `wheels` workflow builds macOS arm64 and Linux x86_64/arm64 wheels, testing them with Python 3.11 and 3.14. It also builds and tests a source distribution. Windows and macOS Intel wheels are not provided.

Publishing requires the GitHub CLI (`gh`) authenticated to the repository and Twine credentials for PyPI. Commit and push changes, then wait for the GitHub `wheels` workflow. To validate upload artifacts without uploading, run from the repository root:

```sh
pypi --check
```

To upload:

```sh
pypi
```

The command requires a clean checkout and downloads **only artifacts from successful CI for the exact current commit**. It verifies three wheels and one sdist for the current version, runs strict Twine checks, then uploads. It uses Twine's normal credentials, such as `~/.pypirc` or `TWINE_USERNAME=__token__` and `TWINE_PASSWORD`. No credentials are stored in this repository. Running `./pypi` from the repository also works without activation.

Bump matching versions in `Cargo.toml` and `pyproject.toml`, update lockfiles, and push before releasing another version. PyPI versions cannot be overwritten. The CI workflow builds and tests; it never publishes automatically.
