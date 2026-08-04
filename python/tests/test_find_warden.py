"""Pin `warden._find_warden.find_warden_bin` against RECORD-based discovery.

Every case writes a real `RECORD` into a real `.dist-info` directory under
`tmp_path` and hands `find_warden_bin` an actual
`importlib.metadata.PathDistribution` built from it, so the suite exercises
stdlib RECORD parsing and `locate_file` semantics rather than a test author's
model of them. The only thing stubbed is the `distribution("warden-cli")`
lookup.
"""

import sys
from importlib.metadata import Distribution, PackageNotFoundError
from pathlib import Path

import pytest

from warden import _find_warden
from warden._find_warden import WardenNotFound, find_warden_bin

# Derived as the code under test derives it, so the suite is decoupled from
# whatever platform it happens to run on.
EXE_NAME = "warden" + (".exe" if sys.platform == "win32" else "")


def _write_record(site_packages: Path, entries: list[str]) -> Path:
    """Write a real RECORD and return the `.dist-info` directory holding it.

    `entries` are RECORD-relative paths. Python 3.12+ drops entries whose file
    does not exist, so callers must create any file they expect to be found.
    """
    dist_info = site_packages / "warden_cli-0.1.0.dist-info"
    dist_info.mkdir(parents=True, exist_ok=True)
    dist_info.joinpath("RECORD").write_text("".join(f"{e},,\n" for e in entries))
    return dist_info


def _patch_lookup(monkeypatch: pytest.MonkeyPatch, dist_info: Path | None) -> None:
    """Point `distribution("warden-cli")` at `dist_info`, or make it raise."""

    def lookup(name: str) -> Distribution:
        if dist_info is None:
            raise PackageNotFoundError(name)
        return Distribution.at(dist_info)

    monkeypatch.setattr(_find_warden, "distribution", lookup)


def _touch(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.touch()
    return path


# --- the two real install shapes verified against actual wheels -------------


def test_venv_install_resolves_relative_record_entry(tmp_path, monkeypatch):
    """`uv pip install` into a venv: RECORD holds `../../../bin/warden` relative
    to the `site-packages` dist-info directory, which must resolve back down to
    the venv's `bin/warden`.
    """
    venv = tmp_path / "venv"
    binary = _touch(venv / "bin" / EXE_NAME)

    dist_info = _write_record(
        venv / "lib" / "python3.12" / "site-packages",
        [f"../../../bin/{EXE_NAME}"],
    )
    _patch_lookup(monkeypatch, dist_info)

    assert find_warden_bin() == str(binary)


def test_target_install_resolves_sibling_bin_entry(tmp_path, monkeypatch):
    """`uv pip install --target X`: RECORD holds `bin/warden` relative to X.

    The unrelated `bin/other-tool` entry sits ahead of the script entry, so the
    basename half of the selection rule is load-bearing here.
    """
    target = tmp_path / "X"
    binary = _touch(target / "bin" / EXE_NAME)
    _touch(target / "bin" / "other-tool")

    dist_info = _write_record(target, ["bin/other-tool", f"bin/{EXE_NAME}"])
    _patch_lookup(monkeypatch, dist_info)

    assert find_warden_bin() == str(binary)


# --- the failure modes ------------------------------------------------------


def test_no_distribution_raises(monkeypatch):
    """Running from a source tree / not installed: distribution() raises."""
    _patch_lookup(monkeypatch, None)

    with pytest.raises(WardenNotFound, match="No installed `warden-cli` distribution"):
        find_warden_bin()


def test_no_record_raises(tmp_path, monkeypatch):
    """A `.dist-info` directory that holds no RECORD file at all."""
    dist_info = tmp_path / "warden_cli-0.1.0.dist-info"
    dist_info.mkdir()
    _patch_lookup(monkeypatch, dist_info)

    with pytest.raises(WardenNotFound, match="no RECORD"):
        find_warden_bin()


def test_no_matching_entry_raises(tmp_path, monkeypatch):
    """RECORD exists but nothing in it satisfies the selection rule.

    This also covers a RECORD-named binary missing from disk: from Python 3.12
    `Distribution.files` filters those out, so they arrive here rather than at
    the file-missing branch.
    """
    site_packages = tmp_path / "site-packages"
    _touch(site_packages / "warden" / "__init__.py")

    dist_info = _write_record(site_packages, ["warden/__init__.py", f"bin/{EXE_NAME}"])
    _patch_lookup(monkeypatch, dist_info)

    with pytest.raises(WardenNotFound, match="No .* script entry"):
        find_warden_bin()


def test_binary_not_named_by_record_is_never_returned(tmp_path, monkeypatch):
    """A real binary on disk that RECORD does not name must never be used.

    This is the escape guarantee: a directory-walking implementation could
    return an unrelated `bin/warden` instead of erroring.
    """
    _touch(tmp_path / "somewhere-else" / "bin" / EXE_NAME)

    dist_info = _write_record(tmp_path / "X", [])
    _patch_lookup(monkeypatch, dist_info)

    with pytest.raises(WardenNotFound):
        find_warden_bin()


def test_package_and_dist_info_entries_are_excluded(tmp_path, monkeypatch):
    """Same-named files under `warden/` and `*.dist-info/` match on basename, so
    the exclusion clause is what keeps the real script entry winning.
    """
    site_packages = tmp_path / "site-packages"
    _touch(site_packages / "warden" / EXE_NAME)
    _touch(site_packages / "warden_cli-0.1.0.dist-info" / EXE_NAME)
    binary = _touch(site_packages / "bin" / EXE_NAME)

    dist_info = _write_record(
        site_packages,
        [
            f"warden/{EXE_NAME}",
            f"warden_cli-0.1.0.dist-info/{EXE_NAME}",
            f"bin/{EXE_NAME}",
        ],
    )
    _patch_lookup(monkeypatch, dist_info)

    assert find_warden_bin() == str(binary)
