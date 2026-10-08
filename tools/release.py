"""Upload only distributions produced by passing CI for the current clean commit."""

from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import tomllib

import fire


def run(arguments, capture=False):
    """
    Command. Run an argument list, report output, and raise on a nonzero exit.

    Args: arguments: executable and arguments; capture: also return printed stdout.
    Returns: stripped stdout when captured, otherwise None. git rev-parse HEAD returns a SHA.
    """
    print("+ " + shlex.join(arguments), flush=True)
    result = subprocess.run(arguments, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    if capture:
        print(result.stdout, end="", flush=True)
        return result.stdout.strip()
    return None


def publish(check=False):
    """
    Command. Download this commit's passing CI artifacts, check them, and optionally upload.

    Args: check: True validates artifacts without uploading anything.
    Returns: None. publish(check=True) prints verified distribution paths; no upload occurs.
    """
    root = Path(run(["git", "rev-parse", "--show-toplevel"], capture=True))
    if Path.cwd().resolve() != root.resolve():
        raise RuntimeError("Run this command from the repository root")
    project = tomllib.loads((root / "pyproject.toml").read_text())["project"]
    if project["name"] != "datafusion_inline_functions":
        raise RuntimeError("This publisher is only for datafusion_inline_functions")
    if run(["git", "status", "--porcelain"], capture=True):
        raise RuntimeError("Commit or remove pending changes before publishing")
    commit = run(["git", "rev-parse", "HEAD"], capture=True)
    run_id = run([
        "gh", "run", "list", "--workflow", "wheels.yml", "--commit", commit,
        "--status", "success", "--json", "databaseId", "--jq", ".[0].databaseId",
    ], capture=True)
    if not run_id or run_id == "null":
        raise RuntimeError("No passing wheels workflow for this commit. Push it and wait for CI.")
    scratch = root / ".scratchpad"
    scratch.mkdir(exist_ok=True)
    destination = Path(tempfile.mkdtemp(prefix="release-", dir=scratch))
    run(["gh", "run", "download", run_id, "--dir", str(destination), "--pattern", "dist-*"])
    distributions = sorted([*destination.rglob("*.whl"), *destination.rglob("*.tar.gz")])
    prefix = f"datafusion_inline_functions-{project['version']}"
    wheels = [path for path in distributions if path.suffix == ".whl"]
    sources = [path for path in distributions if path.name.endswith(".tar.gz")]
    if (len(wheels) != 3 or len(sources) != 1
            or sources[0].name != prefix + ".tar.gz"
            or any(not path.name.startswith(prefix + "-") for path in wheels)):
        raise RuntimeError("Expected three platform wheels and one sdist for the current package version")
    run([sys.executable, "-m", "twine", "check", "--strict", *map(str, distributions)])
    if check:
        print("Verified this commit's CI distributions. Nothing uploaded.")
        return
    run([sys.executable, "-m", "twine", "upload", "--non-interactive", *map(str, distributions)])


if __name__ == "__main__":
    fire.Fire(publish)
