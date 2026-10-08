"""Check publishing decisions without contacting PyPI or GitHub."""

import importlib.util
from pathlib import Path

import pytest


@pytest.fixture
def publisher(tmp_path, monkeypatch):
    """Command. Create a fake clean repository and release runner; return module and recorded commands."""
    spec = importlib.util.spec_from_file_location("release_test_module", Path("tools/release.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    (tmp_path / "pyproject.toml").write_text('[project]\nname="datafusion_inline_functions"\nversion="0.1.0"\n')
    monkeypatch.chdir(tmp_path)
    calls = []

    def fake_run(arguments, capture=False):
        """Command. Record arguments and simulate metadata/downloads; return matching captured stdout."""
        calls.append(arguments)
        if arguments[:3] == ["git", "rev-parse", "--show-toplevel"]:
            return str(tmp_path)
        if arguments[:2] == ["git", "status"]:
            return ""
        if arguments[:3] == ["git", "rev-parse", "HEAD"]:
            return "a" * 40
        if arguments[:3] == ["gh", "run", "list"]:
            return "123"
        if arguments[:3] == ["gh", "run", "download"]:
            destination = Path(arguments[arguments.index("--dir") + 1])
            for platform in ("macosx_11_0_arm64", "manylinux_2_28_x86_64", "manylinux_2_28_aarch64"):
                (destination / f"datafusion_inline_functions-0.1.0-cp311-abi3-{platform}.whl").touch()
            (destination / "datafusion_inline_functions-0.1.0.tar.gz").touch()
        return None

    monkeypatch.setattr(module, "run", fake_run)
    return module, calls


def test_check_never_uploads(publisher):
    """Command. Simulate --check and assert no upload command occurs."""
    module, calls = publisher
    module.publish(check=True)
    assert any("check" in command and "twine" in command for command in calls)
    assert not any("upload" in command for command in calls)


def test_publish_uses_verified_artifacts(publisher):
    """Command. Simulate publishing and assert strict checking precedes a noninteractive upload."""
    module, calls = publisher
    module.publish()
    assert calls[-2][1:5] == ["-m", "twine", "check", "--strict"]
    assert calls[-1][1:5] == ["-m", "twine", "upload", "--non-interactive"]
    assert calls[-1][5:] == calls[-2][5:]


def test_failed_ci_blocks_publish(publisher, monkeypatch):
    """Command. Simulate missing successful CI and assert no artifact download or upload happens."""
    module, calls = publisher
    original = module.run

    def missing_ci(arguments, capture=False):
        """Command. Return no successful CI run while preserving other fake commands."""
        return "null" if arguments[:3] == ["gh", "run", "list"] else original(arguments, capture)

    monkeypatch.setattr(module, "run", missing_ci)
    with pytest.raises(RuntimeError, match="No passing"):
        module.publish()
    assert not any("download" in command or "upload" in command for command in calls)
